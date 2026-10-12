//! Phase 2 spike: boots a Linux kernel to a serial console through
//! Hypervisor.framework.
//!
//! ```sh
//! cargo run -p cave-vm-vmm --bin cave-vmm-spike -- \
//!     --kernel Image --initrd initramfs.cpio.gz [--memory 512] [--cmdline "..."]
//! ```
//!
//! The guest's console is this terminal. Timings go to stderr.

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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use cave_vm_hvf::{Vm, VmConfig};
    use cave_vm_vmm::backend::hvf::HvfBackend;
    use cave_vm_vmm::backend::{Backend, BackendVcpu, Kick};
    use cave_vm_vmm::boot::{self, BootConfig};
    use cave_vm_vmm::runner::{self, Machine};

    const USAGE: &str = "usage: cave-vmm-spike --kernel <Image> [--initrd <file>] [--memory <MiB>] [--cmdline <text>] [--stop-at <text>]";
    const DEFAULT_CMDLINE: &str = "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init";

    struct Args {
        kernel: String,
        initrd: Option<String>,
        memory_mib: u64,
        cmdline: String,
        stop_at: Option<String>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut kernel = None;
        let mut initrd = None;
        let mut memory_mib = 512;
        let mut cmdline = DEFAULT_CMDLINE.to_string();
        let mut stop_at = None;
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
                "-h" | "--help" => return Err(USAGE.to_string()),
                other => return Err(format!("unknown argument {other}\n{USAGE}")),
            }
        }
        Ok(Args {
            kernel: kernel.ok_or_else(|| format!("--kernel is required\n{USAGE}"))?,
            initrd,
            memory_mib,
            cmdline,
            stop_at,
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

    pub fn main() -> Result<(), String> {
        let args = parse_args()?;
        let started = Instant::now();
        let config = BootConfig {
            kernel: read(&args.kernel)?,
            initrd: args.initrd.as_deref().map(read).transpose()?,
            cmdline: args.cmdline,
            memory_size: args.memory_mib << 20,
            cpus: 1,
        };

        let vm = Vm::new(&VmConfig::default()).map_err(|e| e.to_string())?;
        let mut backend = HvfBackend::new(&vm);
        let prepared = boot::prepare(&mut backend, &config).map_err(|e| e.to_string())?;
        let mut vcpu = backend.create_vcpu(0).map_err(|e| e.to_string())?;
        boot::setup_vcpu(&mut vcpu, 0, &prepared.layout).map_err(|e| e.to_string())?;
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
        let mut machine = Machine::new(&mut console, Some(rx), config.cpus).with_stop_flag(stop);
        let result = runner::run(&backend, &mut vcpu, &mut machine);
        drop(machine);

        eprintln!();
        eprintln!("cave-vmm-spike: VM set up in {ready:?}");
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
        let stop = result.map_err(|e| e.to_string())?;
        eprintln!("cave-vmm-spike: stopped: {stop:?}");
        Ok(())
    }
}
