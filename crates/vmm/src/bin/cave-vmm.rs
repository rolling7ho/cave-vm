//! The per-VM process. The daemon starts it with one end of a Unix socket
//! pair as its standard input and speaks the VMM control link over it
//! (`docs/protocol.md`, "VMM control"). Standard output and error go to
//! `vmm.log`: the guest's console, then this process's own notes.
//!
//! It waits for `boot`, boots the kernel with the project disk on
//! `/dev/vda`, the vsock device and, unless `net` is `off`, a network device
//! behind the userspace NAT stack, replies `booted` once the guest agent
//! answers, and then hands out agent connections until `stop`. If the link
//! closes, the VM stops; if the guest powers off, the process exits.

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
    use cave_vm_vmm::nat::{self, ConnectEvent, GuestFrames, HostUdp, Policy, Stack, TcpConnector};
    use cave_vm_vmm::runner::{self, Machine};

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

    struct Booted(vsock::Connector);

    impl RunningVm for Booted {
        fn agent_connection(&self) -> io::Result<UnixStream> {
            self.0.connect(AGENT_PORT)
        }
    }

    /// Boots the VM, then serves the link until `stop` or until it closes.
    /// An `Err` goes back to the daemon as the reply to `boot`.
    fn run(link: &mut UnixStream, config: &VmmConfig) -> Result<(), ErrorMessage> {
        let boot_config = BootConfig {
            kernel: read(&config.kernel, "kernel")?,
            initrd: None,
            cmdline: cmdline(config.net),
            memory_size: config.memory_bytes,
            cpus: config.cpus,
            virtio_slots: 2 + u32::from(config.net != NetMode::Off),
        };
        let block = Block::open(Path::new(&config.disk), false, DISK_ID).map_err(|e| {
            ErrorMessage::new(
                ErrorCode::Io,
                format!("cannot open the disk {}: {e}", config.disk),
            )
        })?;
        let vsock = Vsock::new(GUEST_CID);
        let vm_agent = Booted(vsock.connector());

        let vm = Vm::new(&VmConfig::default()).map_err(|e| failed("cannot create the VM", e))?;
        let mut backend = HvfBackend::new(&vm);
        let prepared = boot::prepare(&mut backend, &boot_config)
            .map_err(|e| failed("cannot load the kernel", e))?;
        let backend = backend;
        let mut machine = Machine::new(io::stdout(), None, config.cpus);
        machine.add_virtio(MmioTransport::new(Box::new(block), prepared.memory.clone()));
        machine.add_virtio(MmioTransport::new(Box::new(vsock), prepared.memory.clone()));
        let mut gateway = network(config.net);
        if let Some((stack, _)) = &gateway {
            let net = Net::new(nat::GUEST_MAC, Box::new(ToStack(stack.guest_frames())));
            let receiver = net.receiver();
            machine.add_virtio(MmioTransport::new(Box::new(net), prepared.memory.clone()));
            gateway = gateway.map(|(stack, _)| (stack, Some(receiver)));
        }
        let machine = machine;

        // When the guest stops by itself, closing our side of the link ends
        // whatever the main thread is waiting for.
        let gone = AtomicBool::new(false);
        let unblock = link
            .try_clone()
            .map_err(|e| failed("cannot clone the control link", e))?;
        let created = Barrier::new(config.cpus as usize + 1);

        thread::scope(|scope| {
            let threads: Vec<_> = (0..config.cpus)
                .map(|index| {
                    let (backend, machine, layout) = (&backend, &machine, &prepared.layout);
                    let (created, gone, unblock) = (&created, &gone, &unblock);
                    scope.spawn(move || {
                        let result = vcpu_thread(backend, machine, layout, index, created);
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
            let closer = gateway.as_ref().map(|(stack, _)| stack.guest_frames());
            if let Some((mut stack, Some(receiver))) = gateway {
                scope.spawn(move || {
                    let mut deliver = |frame: &[u8]| {
                        receiver.deliver(frame);
                    };
                    stack.run(&clock, &mut deliver, &mut record);
                });
            }

            let outcome = serve(link, &vm_agent, &gone);
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

    fn serve(link: &mut UnixStream, vm: &Booted, gone: &AtomicBool) -> Result<(), ErrorMessage> {
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

    fn vcpu_thread<W: io::Write>(
        backend: &HvfBackend<'_>,
        machine: &Machine<W>,
        layout: &BootLayout,
        index: u32,
        created: &Barrier,
    ) -> Result<runner::Stop, String> {
        let vcpu = backend
            .create_vcpu(index)
            .map_err(|e| e.to_string())
            .and_then(|mut vcpu| {
                boot::setup_vcpu(&mut vcpu, index, layout).map_err(|e| e.to_string())?;
                Ok(vcpu)
            });
        created.wait();
        let mut vcpu = vcpu?;
        if index == 0 {
            runner::run(backend, &mut vcpu, machine)
        } else {
            runner::run_secondary(backend, &mut vcpu, index, machine)
        }
        .map_err(|e| e.to_string())
    }
}
