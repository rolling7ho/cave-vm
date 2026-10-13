//! Phase 2 spike: boots a Linux kernel to a serial console through
//! Hypervisor.framework.
//!
//! ```sh
//! cargo run -p cave-vm-vmm --bin cave-vmm-spike -- \
//!     --kernel Image --initrd initramfs.cpio.gz [--memory 512] [--cpus 1] [--cmdline "..."]
//! ```
//!
//! The guest's console is this terminal. Timings go to stderr.
//!
//! `--stop-at <text>` stops the VM when the guest prints `text`; add
//! `--snapshot <dir>` to save it there. `--restore <dir>` runs a saved VM
//! instead of booting (P2.9). `--cpus` sets the number of vCPUs, each on
//! its own thread (default 1); a restore uses the snapshot's count.
//! `--disk <file>` attaches a raw image as a virtio block device (`/dev/vda`
//! in the guest); it cannot be combined with snapshots yet, since device
//! state is not saved until phase 4.

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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, OnceLock, mpsc};
    use std::time::{Duration, Instant};

    use cave_vm_hvf::{Vm, VmConfig};
    use cave_vm_vmm::backend::hvf::HvfBackend;
    use cave_vm_vmm::backend::{Backend, BackendVcpu, Kick};
    use cave_vm_vmm::boot::{self, BootConfig};
    use cave_vm_vmm::devices::virtio::block::Block;
    use cave_vm_vmm::devices::virtio::mmio::MmioTransport;
    use cave_vm_vmm::kernel::BootLayout;
    use cave_vm_vmm::memory::GuestMemory;
    use cave_vm_vmm::runner::{self, Machine, Stop};
    use cave_vm_vmm::snapshot::{self, State, VcpuState};

    const USAGE: &str = "usage: cave-vmm-spike (--kernel <Image> [--initrd <file>] [--memory <MiB>] [--cpus <n>] [--disk <file>] [--cmdline <text>] | --restore <dir>) [--stop-at <text> [--snapshot <dir>]]";
    const DEFAULT_CMDLINE: &str = "console=ttyAMA0 earlycon=pl011,0x9000000 rdinit=/init";

    struct Args {
        kernel: Option<String>,
        initrd: Option<String>,
        memory_mib: u64,
        cpus: u32,
        disk: Option<PathBuf>,
        cmdline: String,
        stop_at: Option<String>,
        snapshot: Option<PathBuf>,
        restore: Option<PathBuf>,
    }

    fn parse_args() -> Result<Args, String> {
        let mut kernel = None;
        let mut initrd = None;
        let mut memory_mib = 512;
        let mut cpus = 1;
        let mut disk = None;
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
                "--cpus" => {
                    cpus = value()?
                        .parse()
                        .ok()
                        .filter(|n| (1..=8).contains(n))
                        .ok_or_else(|| format!("--cpus takes a number from 1 to 8\n{USAGE}"))?
                }
                "--disk" => disk = Some(PathBuf::from(value()?)),
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
        if disk.is_some() && (snapshot.is_some() || restore.is_some()) {
            return Err(format!(
                "--disk cannot be used with --snapshot or --restore yet\n{USAGE}"
            ));
        }
        if snapshot.is_some() && stop_at.is_none() {
            return Err(format!("--snapshot needs --stop-at\n{USAGE}"));
        }
        Ok(Args {
            kernel,
            initrd,
            memory_mib,
            cpus,
            disk,
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
    }

    impl StopAt {
        fn watch(&mut self, buf: &[u8]) {
            if self.seen.is_some() {
                return;
            }
            self.recent.extend_from_slice(buf);
            if self.recent.windows(self.text.len()).any(|w| w == self.text) {
                self.seen = Some(Instant::now());
                // The guest prints from a vCPU thread, which checks the
                // flag as soon as this write returns.
                self.stop.store(true, Ordering::Release);
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

    /// How a vCPU thread sets up its vCPU before running it.
    enum Start<'a> {
        Boot(&'a BootLayout),
        Restore(&'a State),
    }

    /// What a vCPU thread reports when it ends.
    struct Ended {
        result: Result<Stop, String>,
        /// When vCPU 0 started running.
        running: Option<Instant>,
        saved: Option<VcpuState>,
    }

    pub fn main() -> Result<(), String> {
        let args = parse_args()?;
        let started = Instant::now();

        let vm = Vm::new(&VmConfig::default()).map_err(err)?;
        let mut backend = HvfBackend::new(&vm);
        let (memory, layout, saved) = match (&args.kernel, &args.restore) {
            (Some(kernel), _) => {
                let config = BootConfig {
                    kernel: read(kernel)?,
                    initrd: args.initrd.as_deref().map(read).transpose()?,
                    cmdline: args.cmdline.clone(),
                    memory_size: args.memory_mib << 20,
                    cpus: args.cpus,
                    virtio_slots: u32::from(args.disk.is_some()),
                };
                let prepared = boot::prepare(&mut backend, &config).map_err(err)?;
                (prepared.memory, Some(prepared.layout), None)
            }
            (None, Some(dir)) => {
                let state = snapshot::load_state(dir).map_err(err)?;
                backend.create_irqchip().map_err(err)?;
                let memory = snapshot::load_memory(dir, state.memory_size).map_err(err)?;
                backend.map_memory(&memory).map_err(err)?;
                (memory, None, Some(state))
            }
            (None, None) => unreachable!("parse_args requires one"),
        };
        let backend = backend;
        let cpus = saved.as_ref().map_or(args.cpus, |s| s.vcpus.len() as u32);
        let start = match (&layout, &saved) {
            (Some(layout), _) => Start::Boot(layout),
            (None, Some(state)) => Start::Restore(state),
            (None, None) => unreachable!(),
        };

        let stop = Arc::new(AtomicBool::new(false));
        let mut console = Console {
            started,
            first_output: None,
            stop_at: args.stop_at.map(|text| StopAt {
                text: text.into_bytes(),
                recent: Vec::new(),
                seen: None,
                stop: Arc::clone(&stop),
            }),
        };
        let (tx, rx) = mpsc::channel();
        let mut machine = Machine::new(&mut console, Some(rx), cpus).with_stop_flag(stop);
        if let Some(path) = &args.disk {
            let block = Block::open(path, false, "cave-spike")
                .map_err(|e| format!("could not open {}: {e}", path.display()))?;
            machine.add_virtio(MmioTransport::new(Box::new(block), memory.clone()));
        }
        let machine = machine;
        if let Some(state) = &saved {
            machine.devices().uart.set_state(&state.uart);
            let power: Vec<_> = state.vcpus.iter().map(|v| v.power).collect();
            machine.set_power(&power);
        }

        // Every vCPU exists before the GIC state is restored, and the GIC
        // state is restored before any vCPU state (docs/spike/p2.9).
        let created = Barrier::new(cpus as usize + 1);
        let gic_ready = Barrier::new(cpus as usize + 1);
        // When snapshotting, vCPUs stay alive until the GIC state is saved.
        let saving = args.snapshot.is_some();
        let vcpus_saved = Barrier::new(cpus as usize + 1);
        let gic_saved = Barrier::new(cpus as usize + 1);
        let (kick_tx, kick_rx) = mpsc::channel::<Kick>();
        // One host time for every vCPU's timer at restore, and another at
        // save, so the guest counters keep the same relation to each other.
        // They must be separate: a restored VM that is saved again would
        // otherwise save with the restore time and lose the run in between.
        let clocks = Clocks::default();

        let (ended, irqchip, power) = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..cpus)
                .map(|index| {
                    let (backend, machine, start) = (&backend, &machine, &start);
                    let (created, gic_ready) = (&created, &gic_ready);
                    let (vcpus_saved, gic_saved) = (&vcpus_saved, &gic_saved);
                    let kick_tx = kick_tx.clone();
                    let clocks = &clocks;
                    scope.spawn(move || {
                        let ended = vcpu_thread(
                            backend, machine, start, index, created, gic_ready, kick_tx, saving,
                            clocks,
                        );
                        if ended.result.is_err() {
                            machine.request_stop();
                        }
                        if saving {
                            vcpus_saved.wait();
                            gic_saved.wait();
                        }
                        ended
                    })
                })
                .collect();
            drop(kick_tx);

            created.wait();
            let restored = match &start {
                Start::Restore(state) => backend.set_irqchip_state(&state.irqchip).map_err(err),
                Start::Boot(_) => Ok(()),
            };
            if restored.is_err() {
                machine.request_stop();
            }
            gic_ready.wait();

            if let Ok(kick) = kick_rx.recv() {
                spawn_stdin(tx, kick);
            }

            let mut irqchip = Ok(Vec::new());
            let mut power = Vec::new();
            if saving {
                vcpus_saved.wait();
                irqchip = backend.irqchip_state().map_err(err);
                power = machine.power();
                gic_saved.wait();
            }
            let ended: Vec<Ended> = threads
                .into_iter()
                .map(|t| t.join().expect("vCPU thread panicked"))
                .collect();
            restored.map(|()| (ended, irqchip, power))
        })?;
        let uart = machine.devices().uart.state();
        drop(machine);

        eprintln!();
        let what = if saved.is_some() {
            "restored"
        } else {
            "set up"
        };
        let running = ended[0].running.map(|t| t - started);
        eprintln!(
            "cave-vmm-spike: VM with {cpus} vCPU(s) {what} in {:?}",
            running.unwrap_or(Duration::ZERO)
        );
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
        let mut states = Vec::new();
        for (index, e) in ended.into_iter().enumerate() {
            let stopped = e.result.map_err(|m| format!("vCPU {index}: {m}"))?;
            if index == 0 {
                eprintln!("cave-vmm-spike: stopped: {stopped:?}");
            }
            states.extend(e.saved);
        }

        if let Some(dir) = &args.snapshot {
            if states.len() != cpus as usize {
                return Err("the VM did not stop at --stop-at, so nothing was saved".into());
            }
            for (state, power) in states.iter_mut().zip(power) {
                state.power = power;
            }
            save_snapshot(dir, irqchip?, states, &memory, uart)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn vcpu_thread<W: Write>(
        backend: &HvfBackend<'_>,
        machine: &Machine<W>,
        start: &Start<'_>,
        index: u32,
        created: &Barrier,
        gic_ready: &Barrier,
        kick_tx: mpsc::Sender<Kick>,
        saving: bool,
        clocks: &Clocks,
    ) -> Ended {
        let mut ended = Ended {
            result: Ok(Stop::Canceled),
            running: None,
            saved: None,
        };
        let vcpu = backend
            .create_vcpu(index)
            .map_err(err)
            .and_then(|mut vcpu| {
                match start {
                    Start::Boot(layout) => boot::setup_vcpu(&mut vcpu, index, layout),
                    Start::Restore(state) => {
                        snapshot::restore_mpidr(&mut vcpu, &state.vcpus[index as usize])
                    }
                }
                .map_err(err)?;
                Ok(vcpu)
            });
        created.wait();
        gic_ready.wait();
        let mut vcpu = match vcpu {
            Ok(vcpu) => vcpu,
            Err(e) => {
                ended.result = Err(e);
                return ended;
            }
        };
        if let Start::Restore(state) = start {
            let saved = &state.vcpus[index as usize];
            let now = *clocks.restore.get_or_init(|| backend.now_ticks());
            if let Err(e) = snapshot::restore_vcpu(&mut vcpu, saved, now) {
                ended.result = Err(err(e));
                return ended;
            }
        }
        if index == 0 {
            let _ = kick_tx.send(vcpu.kicker());
            ended.running = Some(Instant::now());
        }
        drop(kick_tx);
        ended.result = if index == 0 {
            runner::run(backend, &mut vcpu, machine)
        } else {
            runner::run_secondary(backend, &mut vcpu, index, machine)
        }
        .map_err(err);
        if saving && ended.result == Ok(Stop::Canceled) {
            let now = *clocks.save.get_or_init(|| backend.now_ticks());
            match snapshot::save_vcpu(&vcpu, now) {
                Ok(state) => ended.saved = Some(state),
                Err(e) => ended.result = Err(err(e)),
            }
        }
        ended
    }

    /// Host times shared by every vCPU thread: one taken at restore, one at
    /// save.
    #[derive(Default)]
    struct Clocks {
        restore: OnceLock<u64>,
        save: OnceLock<u64>,
    }

    /// Forwards the terminal to the guest's UART, kicking vCPU 0 so it
    /// notices.
    fn spawn_stdin(tx: mpsc::Sender<Vec<u8>>, kick: Kick) {
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
    }

    fn save_snapshot(
        dir: &Path,
        irqchip: Vec<u8>,
        vcpus: Vec<VcpuState>,
        memory: &GuestMemory,
        uart: cave_vm_vmm::devices::pl011::Pl011State,
    ) -> Result<(), String> {
        let started = Instant::now();
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let state = State {
            memory_size: memory.regions().iter().map(|r| r.size() as u64).sum(),
            irqchip,
            uart,
            vcpus,
        };
        snapshot::save(dir, &state, memory).map_err(err)?;
        eprintln!(
            "cave-vmm-spike: snapshot written to {} in {:?}",
            dir.display(),
            started.elapsed()
        );
        Ok(())
    }
}
