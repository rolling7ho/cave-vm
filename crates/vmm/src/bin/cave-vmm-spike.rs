//! Phase 2 spike: boots a Linux kernel to a serial console through
//! Hypervisor.framework.
//!
//! ```sh
//! cargo run -p cave-vm-vmm --bin cave-vmm-spike -- \
//!     --kernel Image --initrd initramfs.cpio.gz [--memory 512] [--cmdline "..."]
//! ```
//!
//! The guest's console is this terminal. Timings go to stderr.
//!
//! `--stop-at <text>` stops the VM when the guest prints `text`; add
//! `--snapshot <dir>` to save it there. `--restore <dir>` runs a saved VM
//! instead of booting (P2.9).

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() {
    if let Err(message) = spike::main() {
        eprintln!("cave-vmm-spike: {message}");
        std::process::exit(1);
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("cave-vmm-spike needs an Apple Silicon Mac");
    std::process::exit(1);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod spike {
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use cave_vm_hvf::{Vm, VmConfig};
    use cave_vm_vmm::backend::hvf::HvfBackend;
    use cave_vm_vmm::backend::{Backend, BackendVcpu, Kick};
    use cave_vm_vmm::boot::{self, BootConfig};
    use cave_vm_vmm::runner::{self, Machine, Stop};
    use cave_vm_vmm::snapshot::{self, State};

    const USAGE: &str = "usage: cave-vmm-spike (--kernel <Image> [--initrd <file>] [--memory <MiB>] [--cmdline <text>] | --restore <dir>) [--stop-at <text> [--snapshot <dir>]]";
    const DEFAULT_CMDLINE: &str = "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init";

    struct Args {
        kernel: Option<String>,
        initrd: Option<String>,
        memory_mib: u64,
        cmdline: String,
        stop_at: Option<String>,
        snapshot: Option<PathBuf>,
        restore: Option<PathBuf>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut kernel = None;
        let mut initrd = None;
        let mut memory_mib = 512;
        let mut cmdline = DEFAULT_CMDLINE.to_string();
        let mut stop_at = None;
        let mut snapshot = None;
        let mut restore = None;
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
            };
            match flag.as_str() {
                "--kernel" => kernel = Some(value()?),
                "--initrd" => initrd = Some(value()?),
                "--memory" => {
                    memory_mib = value()?
                        .parse()
                        .map_err(|_| format!("--memory takes a whole number of MiB\n{USAGE}"))?
                }
                "--cmdline" => cmdline = value()?,
                "--stop-at" => stop_at = Some(value()?),
                "--snapshot" => snapshot = Some(PathBuf::from(value()?)),
                "--restore" => restore = Some(PathBuf::from(value()?)),
                "-h" | "--help" => return Err(USAGE.to_string()),
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            }
        }
        if kernel.is_some() == restore.is_some() {
            return Err(format!("give either --kernel or --restore\n{USAGE}"));
        }
        if snapshot.is_some() && stop_at.is_none() {
            return Err(format!("--snapshot needs --stop-at\n{USAGE}"));
        }
        Ok(Args {
            kernel,
            initrd,
            memory_mib,
            cmdline,
            stop_at,
            snapshot,
            restore,
        })
    }

    fn read(path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| format!("could not read {path}: {e}"))
    }

    /// Forwards what the guest prints, noting when the first byte arrives
    /// and, with `--stop-at`, stopping the VM when the guest prints a text.
    struct Console {
        started: Instant,
        first_output: Option<Instant>,
        stop_at: Option<StopAt>,
    }

    struct StopAt {
        text: Vec<u8>,
        recent: Vec<u8>,
        seen: Option<Instant>,
        stop: Arc<AtomicBool>,
        kick: Kick,
    }

    impl StopAt {
        fn watch(&mut self, buf: &[u8]) {
            if self.seen.is_some() {
                return;
            }
            self.recent.extend_from_slice(buf);
            if self.recent.windows(self.text.len()).any(|w| w == self.text) {
                self.seen = Some(Instant::now());
                self.stop.store(true, Ordering::Release);
                (self.kick)();
            }
            let keep = self.recent.len().saturating_sub(self.text.len());
            self.recent.drain(..keep);
        }
    }

    impl Write for Console {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.first_output.get_or_insert_with(Instant::now);
            if let Some(stop_at) = &mut self.stop_at {
                stop_at.watch(buf);
            }
            io::stdout().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            io::stdout().flush()
        }
    }

    fn err(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    pub fn main() -> Result<(), String> {
        let args = parse_args()?;
        let started = Instant::now();

        let vm = Vm::new(&VmConfig::default()).map_err(err)?;
        let mut backend = HvfBackend::new(&vm);
        let (mut vcpu, memory, saved) = match (&args.kernel, &args.restore) {
            (Some(kernel), _) => {
                let config = BootConfig {
                    kernel: read(kernel)?,
                    initrd: args.initrd.as_deref().map(read).transpose()?,
                    cmdline: args.cmdline.clone(),
                    memory_size: args.memory_mib << 20,
                    cpus: 1,
                };
                let prepared = boot::prepare(&mut backend, &config).map_err(err)?;
                let mut vcpu = backend.create_vcpu(0).map_err(err)?;
                boot::setup_vcpu(&mut vcpu, 0, &prepared.layout).map_err(err)?;
                (vcpu, prepared.memory, None)
            }
            (None, Some(dir)) => {
                let state = snapshot::load_state(dir).map_err(err)?;
                backend.create_irqchip().map_err(err)?;
                let memory = snapshot::load_memory(dir, state.memory_size).map_err(err)?;
                backend.map_memory(&memory).map_err(err)?;
                let mut vcpus = [backend.create_vcpu(0).map_err(err)?];
                snapshot::restore(&backend, &mut vcpus, &state).map_err(err)?;
                let [vcpu] = vcpus;
                (vcpu, memory, Some(state))
            }
            (None, None) => unreachable!("parse_args requires one"),
        };
        let ready = started.elapsed();

        let (tx, rx) = mpsc::channel();
        let kick = vcpu.kicker();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let mut stdin = io::stdin();
            while let Ok(n) = stdin.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
                kick();
            }
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut console = Console {
            started,
            first_output: None,
            stop_at: args.stop_at.map(|text| StopAt {
                text: text.into_bytes(),
                recent: Vec::new(),
                seen: None,
                stop: Arc::clone(&stop),
                kick: vcpu.kicker(),
            }),
        };
        let mut machine = Machine::new(&mut console, Some(rx), 1).with_stop_flag(stop);
        if let Some(state) = &saved {
            machine.uart.set_state(&state.uart);
        }
        let result = runner::run(&backend, &mut vcpu, &mut machine);
        let uart = machine.uart.state();
        drop(machine);

        eprintln!();
        let what = if saved.is_some() {
            "restored"
        } else {
            "set up"
        };
        eprintln!("cave-vmm-spike: VM {what} in {ready:?}");
        if let Some(first) = console.first_output {
            eprintln!(
                "cave-vmm-spike: first console output after {:?}",
                first - console.started
            );
        }
        if let Some(seen) = console.stop_at.as_ref().and_then(|s| s.seen) {
            eprintln!("cave-vmm-spike: --stop-at text after {:?}", seen - started);
        }
        eprintln!("cave-vmm-spike: ran for {:?}", started.elapsed());
        let stopped = result.map_err(err)?;
        eprintln!("cave-vmm-spike: stopped: {stopped:?}");

        if let (Some(dir), Stop::Canceled) = (&args.snapshot, stopped) {
            save_snapshot(dir, &backend, &vcpu, &memory, uart)?;
        }
        Ok(())
    }

    fn save_snapshot<B: Backend, V: BackendVcpu>(
        dir: &Path,
        backend: &B,
        vcpu: &V,
        memory: &cave_vm_vmm::memory::GuestMemory,
        uart: cave_vm_vmm::devices::pl011::Pl011State,
    ) -> Result<(), String> {
        let started = Instant::now();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let state = State {
            memory_size: memory.regions().iter().map(|r| r.size() as u64).sum(),
            irqchip: backend.irqchip_state().map_err(err)?,
            uart,
            vcpus: vec![snapshot::save_vcpu(vcpu, backend.now_ticks()).map_err(err)?],
        };
        let captured = started.elapsed();
        snapshot::save(dir, &state, memory).map_err(err)?;
        eprintln!(
            "cave-vmm-spike: snapshot state captured in {captured:?}, written to {} in {:?}",
            dir.display(),
            started.elapsed()
        );
        Ok(())
    }
}
