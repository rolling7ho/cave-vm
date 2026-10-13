//! The per-VM process. The daemon starts it with one end of a Unix socket
//! pair as its standard input and speaks the VMM control link over it
//! (`docs/protocol.md`, "VMM control"). Standard output and error go to
//! `vmm.log`: the guest's console, then this process's own notes.
//!
//! It waits for `boot`, boots the kernel with the project disk on
//! `/dev/vda`, the vsock device and, unless `net` is `off`, a network device
//! behind the userspace NAT stack, replies `booted` once the guest agent
//! answers, and then hands out agent connections and takes snapshots until
//! `stop`. A `boot` that names a snapshot restores it instead, with guest
//! memory mapped copy-on-write from the snapshot's `memory` file. If the
//! link closes, the VM stops; if the guest powers off, the process exits.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn main() -> std::process::ExitCode {
    vmm::main()
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn main() {
    eprintln!("cave-vmm needs an Apple Silicon Mac");
    std::process::exit(1);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod vmm {
    use std::io;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::process::ExitCode;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use cave_vm_hvf::{Vm, VmConfig};
    use cave_vm_proto::daemon::NetMode;
    use cave_vm_proto::vmm::{FromVmm, VmmConfig};
    use cave_vm_proto::{AGENT_PORT, ErrorCode, ErrorMessage};
    use cave_vm_vmm::backend::Backend;
    use cave_vm_vmm::backend::hvf::HvfBackend;
    use cave_vm_vmm::boot::{self, BootConfig};
    use cave_vm_vmm::control::{self, RunningVm};
    use cave_vm_vmm::devices::virtio::block::Block;
    use cave_vm_vmm::devices::virtio::mmio::MmioTransport;
    use cave_vm_vmm::devices::virtio::net::{Net, NetBackend, NetReceiver};
    use cave_vm_vmm::devices::virtio::vsock::{self, Vsock};
    use cave_vm_vmm::host;
    use cave_vm_vmm::kernel::BootLayout;
    use cave_vm_vmm::memory::GuestMemory;
    use cave_vm_vmm::nat::{self, ConnectEvent, GuestFrames, HostUdp, Policy, Stack, TcpConnector};
    use cave_vm_vmm::runner::{self, Machine};
    use cave_vm_vmm::snapshot::{self, State};

    /// The guest's vsock context ID: the first one free for guests.
    const GUEST_CID: u64 = 3;
    /// How long the guest has to boot and start its agent.
    const AGENT_TIMEOUT: Duration = Duration::from_secs(50);
    /// The disk's serial number, which the guest sees in
    /// `/sys/block/vda/serial`.
    const DISK_ID: &str = "cave-project";

    fn note(message: impl std::fmt::Display) {
        eprintln!("cave-vmm: {message}");
    }

    pub fn main() -> ExitCode {
        let mut link = match host::stdin_socket() {
            Ok(link) => link,
            Err(e) => {
                note(format!(
                    "standard input is not the daemon's socket ({e}); \
                     cave-vmm is started by cave-daemon, not by hand"
                ));
                return ExitCode::FAILURE;
            }
        };
        let config = match control::wait_for_boot(&mut link) {
            Ok(Some(config)) => config,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                note(format!("control link failed: {e}"));
                return ExitCode::FAILURE;
            }
        };
        match run(&mut link, &config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                note(&e.message);
                let _ = control::reply(&mut link, &FromVmm::Error(e));
                ExitCode::FAILURE
            }
        }
    }

    fn failed(context: &str, e: impl std::fmt::Display) -> ErrorMessage {
        ErrorMessage::new(ErrorCode::Internal, format!("{context}: {e}"))
    }

    fn read(path: &str, what: &str) -> Result<Vec<u8>, ErrorMessage> {
        std::fs::read(path).map_err(|e| {
            ErrorMessage::new(
                ErrorCode::NotFound,
                format!("cannot read the {what} {path}: {e}"),
            )
        })
    }

    /// The kernel command line: the console for `vmm.log`, the project disk
    /// as the root file system, a reset rather than a hang on a kernel panic
    /// (so a failed boot is reported at once), and the network address when
    /// there is one.
    fn cmdline(net: NetMode) -> String {
        let mut line = String::from("console=ttyAMA0 root=/dev/vda rw panic=-1");
        if net != NetMode::Off {
            line.push(' ');
            line.push_str(&nat::kernel_ip_arg());
        }
        line
    }

    /// How the vCPUs start.
    enum Start<'a> {
        Boot(&'a BootLayout),
        Restore(&'a State),
    }

    struct Booted<'a, B> {
        connector: vsock::Connector,
        machine: &'a Machine<io::Stdout>,
        backend: &'a B,
        memory: &'a GuestMemory,
        disk: &'a Path,
    }

    impl<B: Backend> RunningVm for Booted<'_, B> {
        fn agent_connection(&self) -> io::Result<UnixStream> {
            self.connector.connect(AGENT_PORT)
        }

        fn snapshot(&self, dir: &Path) -> Result<(), ErrorMessage> {
            let started = Instant::now();
            // Best effort: memory and disk are saved together, so a restore
            // of both is consistent anyway; the flush is for a restore that
            // can only use the disk.
            if let Err(e) = self.agent_connection().and_then(control::prepare_snapshot) {
                note(format!(
                    "the guest did not flush its disk before the snapshot ({e})"
                ));
            }
            snapshot::take(dir, self.machine, self.backend, self.memory, self.disk).map_err(
                |e| ErrorMessage::new(ErrorCode::Io, format!("cannot snapshot the VM: {e}")),
            )?;
            note(format!(
                "snapshot written to {} in {:?}",
                dir.display(),
                started.elapsed()
            ));
            Ok(())
        }
    }

    /// Reads a snapshot to restore and checks it fits the VM asked for.
    fn load_snapshot(dir: &Path, config: &VmmConfig) -> Result<State, ErrorMessage> {
        let state = snapshot::load_state(dir).map_err(|e| {
            ErrorMessage::new(
                ErrorCode::BadRequest,
                format!("cannot restore {}: {e}", dir.display()),
            )
        })?;
        if state.vcpus.len() != config.cpus as usize || state.memory_size != config.memory_bytes {
            return Err(ErrorMessage::new(
                ErrorCode::BadRequest,
                format!(
                    "the snapshot has {} vCPUs and {} bytes of memory, the VM asked for {} and {}",
                    state.vcpus.len(),
                    state.memory_size,
                    config.cpus,
                    config.memory_bytes
                ),
            ));
        }
        Ok(state)
    }

    /// Boots the VM, then serves the link until `stop` or until it closes.
    /// An `Err` goes back to the daemon as the reply to `boot`.
    fn run(link: &mut UnixStream, config: &VmmConfig) -> Result<(), ErrorMessage> {
        let saved = config
            .restore
            .as_deref()
            .map(|dir| load_snapshot(Path::new(dir), config))
            .transpose()?;
        let block = Block::open(Path::new(&config.disk), false, DISK_ID).map_err(|e| {
            ErrorMessage::new(
                ErrorCode::Io,
                format!("cannot open the disk {}: {e}", config.disk),
            )
        })?;
        let vsock = Vsock::new(GUEST_CID);
        let connector = vsock.connector();

        let vm = Vm::new(&VmConfig::default()).map_err(|e| failed("cannot create the VM", e))?;
        let mut backend = HvfBackend::new(&vm);
        let (memory, layout) = match (&saved, &config.restore) {
            (Some(state), Some(dir)) => {
                backend
                    .create_irqchip()
                    .map_err(|e| failed("cannot create the interrupt controller", e))?;
                let memory = snapshot::load_memory(Path::new(dir), state.memory_size)
                    .map_err(|e| failed("cannot map the snapshot's memory", e))?;
                backend
                    .map_memory(&memory)
                    .map_err(|e| failed("cannot map guest memory", e))?;
                (memory, None)
            }
            _ => {
                let boot_config = BootConfig {
                    kernel: read(&config.kernel, "kernel")?,
                    initrd: None,
                    cmdline: cmdline(config.net),
                    memory_size: config.memory_bytes,
                    cpus: config.cpus,
                    virtio_slots: 2 + u32::from(config.net != NetMode::Off),
                };
                let prepared = boot::prepare(&mut backend, &boot_config)
                    .map_err(|e| failed("cannot load the kernel", e))?;
                (prepared.memory, Some(prepared.layout))
            }
        };
        let backend = backend;
        let start = match (&layout, &saved) {
            (Some(layout), _) => Start::Boot(layout),
            (None, Some(state)) => Start::Restore(state),
            (None, None) => unreachable!("a VM either boots or restores"),
        };
        let mut machine = Machine::new(io::stdout(), None, config.cpus);
        machine.add_virtio(MmioTransport::new(Box::new(block), memory.clone()));
        machine.add_virtio(MmioTransport::new(Box::new(vsock), memory.clone()));
        let mut gateway = network(config.net);
        if let Some((stack, _)) = &gateway {
            let net = Net::new(nat::GUEST_MAC, Box::new(ToStack(stack.guest_frames())));
            let receiver = net.receiver();
            machine.add_virtio(MmioTransport::new(Box::new(net), memory.clone()));
            gateway = gateway.map(|(stack, _)| (stack, Some(receiver)));
        }
        let machine = machine;
        if let Some(state) = &saved {
            let mut devices = machine.devices();
            devices.uart.set_state(&state.uart);
            devices.restore_virtio(&state.devices).map_err(|e| {
                ErrorMessage::new(ErrorCode::BadRequest, format!("cannot restore: {e}"))
            })?;
            drop(devices);
            let power: Vec<_> = state.vcpus.iter().map(|v| v.power).collect();
            machine.set_power(&power);
        }
        let vm_agent = Booted {
            connector,
            machine: &machine,
            backend: &backend,
            memory: &memory,
            disk: Path::new(&config.disk),
        };

        // When the guest stops by itself, closing our side of the link ends
        // whatever the main thread is waiting for.
        let gone = AtomicBool::new(false);
        let unblock = link
            .try_clone()
            .map_err(|e| failed("cannot clone the control link", e))?;
        // Every vCPU exists before the interrupt controller state is
        // restored, and that comes before any vCPU state (docs/spike/p2.9).
        let created = Barrier::new(config.cpus as usize + 1);
        let gic_ready = Barrier::new(config.cpus as usize + 1);
        // One host time for every vCPU's timer, so the guest counters keep
        // their relation to each other.
        let restore_now = OnceLock::new();

        thread::scope(|scope| {
            let threads: Vec<_> = (0..config.cpus)
                .map(|index| {
                    let (backend, machine, start) = (&backend, &machine, &start);
                    let (created, gic_ready, restore_now) = (&created, &gic_ready, &restore_now);
                    let (gone, unblock) = (&gone, &unblock);
                    scope.spawn(move || {
                        let threads = Threads {
                            created,
                            gic_ready,
                            restore_now,
                        };
                        let result = vcpu_thread(backend, machine, start, index, &threads);
                        match &result {
                            Err(e) => note(format!("vCPU {index}: {e}")),
                            Ok(stop) if index == 0 => note(format!("the guest stopped: {stop:?}")),
                            Ok(_) => {}
                        }
                        if index == 0 || result.is_err() {
                            gone.store(true, Ordering::Release);
                            machine.request_stop();
                            let _ = unblock.shutdown(Shutdown::Read);
                        }
                    })
                })
                .collect();
            created.wait();
            let restored = match &start {
                Start::Restore(state) => backend
                    .set_irqchip_state(&state.irqchip)
                    .map_err(|e| failed("cannot restore the interrupt controller", e)),
                Start::Boot(_) => Ok(()),
            };
            if restored.is_err() {
                machine.request_stop();
            }
            gic_ready.wait();
            let closer = gateway.as_ref().map(|(stack, _)| stack.guest_frames());
            if let Some((mut stack, Some(receiver))) = gateway {
                scope.spawn(move || {
                    let mut deliver = |frame: &[u8]| {
                        receiver.deliver(frame);
                    };
                    stack.run(&clock, &mut deliver, &mut record);
                });
            }

            let outcome = restored.and_then(|()| serve(link, &vm_agent, &gone));
            machine.request_stop();
            if let Some(closer) = closer {
                closer.close();
            }
            for thread in threads {
                thread.join().expect("vCPU thread panicked");
            }
            outcome
        })
    }

    /// The network stack for `mode`, or none for `off`, with an empty slot
    /// for the device's receiver.
    fn network(mode: NetMode) -> Option<(Stack, Option<NetReceiver>)> {
        let mode = match mode {
            NetMode::Off => return None,
            NetMode::Nat => nat::Mode::Nat,
            NetMode::Lan => nat::Mode::Lan,
        };
        // The Mac's own addresses are denied in every mode.
        let host_addrs = host::ipv4_addresses().unwrap_or_else(|e| {
            note(format!(
                "cannot list the Mac's addresses ({e}); denying loopback only"
            ));
            Vec::new()
        });
        let stack = Stack::new(
            clock(),
            Policy::new(mode, host_addrs),
            Arc::new(TcpConnector::default()),
        )
        .with_udp(Arc::new(HostUdp), nat::host_dns_servers());
        Some((stack, None))
    }

    /// Time since this process started, the network stack's clock.
    fn clock() -> Duration {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }

    /// Connection attempts go to `vmm.log` until the daemon's audit log
    /// takes them.
    fn record(event: ConnectEvent) {
        let verdict = match event.reason {
            None if event.allowed => "allowed".to_string(),
            None => "denied".to_string(),
            Some(reason) => format!("denied ({reason})"),
        };
        note(format!("{:?} to {} {verdict}", event.protocol, event.dst));
    }

    /// Frames from the guest go to the network stack's thread.
    struct ToStack(GuestFrames);

    impl NetBackend for ToStack {
        fn send(&mut self, frame: &[u8]) {
            self.0.send(frame);
        }
    }

    fn serve<B: Backend>(
        link: &mut UnixStream,
        vm: &Booted<'_, B>,
        gone: &AtomicBool,
    ) -> Result<(), ErrorMessage> {
        let connect = || vm.agent_connection();
        // Stop waiting if the guest stops or the daemon goes away.
        let stopped = || gone.load(Ordering::Acquire) || host::hung_up(link);
        control::wait_for_agent(&connect, &stopped, AGENT_TIMEOUT)
            .map_err(|e| e.with_fix("the guest's console is in vmm.log in the VM's folder"))?;
        note("the guest agent answered");
        control::reply(link, &FromVmm::Booted).map_err(|e| failed("control link", e))?;
        match control::serve(link, vm) {
            Ok(ending) => note(format!("stopping: {ending:?}")),
            // The guest stopped and shut the link, or the daemon went away.
            Err(e) if gone.load(Ordering::Acquire) => note(format!("the VM stopped ({e})")),
            Err(e) => note(format!("control link failed: {e}")),
        }
        Ok(())
    }

    /// What the vCPU threads share while they start.
    struct Threads<'a> {
        created: &'a Barrier,
        gic_ready: &'a Barrier,
        restore_now: &'a OnceLock<u64>,
    }

    fn vcpu_thread<W: io::Write>(
        backend: &HvfBackend<'_>,
        machine: &Machine<W>,
        start: &Start<'_>,
        index: u32,
        threads: &Threads<'_>,
    ) -> Result<runner::Stop, String> {
        let vcpu = backend
            .create_vcpu(index)
            .map_err(|e| e.to_string())
            .and_then(|mut vcpu| {
                match start {
                    Start::Boot(layout) => boot::setup_vcpu(&mut vcpu, index, layout),
                    Start::Restore(state) => {
                        snapshot::restore_mpidr(&mut vcpu, &state.vcpus[index as usize])
                    }
                }
                .map_err(|e| e.to_string())?;
                Ok(vcpu)
            });
        threads.created.wait();
        threads.gic_ready.wait();
        let mut vcpu = vcpu?;
        if let Start::Restore(state) = start {
            let now = *threads.restore_now.get_or_init(|| backend.now_ticks());
            snapshot::restore_vcpu(&mut vcpu, &state.vcpus[index as usize], now)
                .map_err(|e| e.to_string())?;
        }
        if index == 0 {
            runner::run(backend, &mut vcpu, machine)
        } else {
            runner::run_secondary(backend, &mut vcpu, index, machine)
        }
        .map_err(|e| e.to_string())
    }
}
