use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

use cave_vm_proto::{VERSIONS, read_frame, write_frame};

use super::*;

/// A free Mac port, found by binding port 0.
fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A fake agent per connection: answers hello, records the port asked for,
/// then echoes raw bytes upper-cased, or refuses when `refuse` is set.
fn fake_agents(asked: Arc<Mutex<Vec<u16>>>, refuse: bool) -> Connect {
    Arc::new(move || {
        let (ours, mut agent) = UnixStream::pair().unwrap();
        let asked = Arc::clone(&asked);
        thread::spawn(move || {
            let Ok(Some(Message::Hello(hello))) = read_frame::<_, Message>(&mut agent) else {
                return;
            };
            let reply = hello.accept(VERSIONS, "fake-agent").unwrap();
            write_frame(&mut agent, &Message::Hello(reply)).unwrap();
            let Ok(Some(Message::PortForward { port })) = read_frame::<_, Message>(&mut agent)
            else {
                return;
            };
            asked.lock().unwrap().push(port);
            if refuse {
                let e = ErrorMessage::new(ErrorCode::NotFound, "nothing is listening");
                write_frame(&mut agent, &Message::Error(e)).unwrap();
                return;
            }
            write_frame(&mut agent, &Message::PortForwardOk).unwrap();
            let mut buf = [0u8; 1024];
            loop {
                match agent.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let upper = buf[..n].to_ascii_uppercase();
                        if agent.write_all(&upper).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Ok(ours)
    })
}

#[test]
fn connections_to_the_mac_port_reach_the_vm_port() {
    let mac = free_port();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let forward = bind(PortMap { mac, vm: 80 })
        .unwrap()
        .serve(fake_agents(Arc::clone(&asked), false));

    for _ in 0..2 {
        let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, mac)).unwrap();
        tcp.write_all(b"hello").unwrap();
        tcp.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        tcp.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"HELLO");
    }
    assert_eq!(*asked.lock().unwrap(), [80, 80]);

    // Dropping the forward closes the Mac port.
    drop(forward);
    thread::sleep(ACCEPT_POLL * 4);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, mac)).is_err());
}

#[test]
fn a_vm_port_with_nothing_listening_closes_the_mac_side() {
    let mac = free_port();
    let _forward = bind(PortMap { mac, vm: 81 })
        .unwrap()
        .serve(fake_agents(Arc::new(Mutex::new(Vec::new())), true));
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, mac)).unwrap();
    let mut got = Vec::new();
    let _ = tcp.read_to_end(&mut got);
    assert!(got.is_empty());
}

#[test]
fn a_mac_port_in_use_says_how_to_pick_another() {
    let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mac = taken.local_addr().unwrap().port();
    let Err(e) = bind(PortMap { mac, vm: 3000 }) else {
        panic!("bound a port in use");
    };
    assert_eq!(
        e.message,
        format!("port {mac} on this Mac is already in use")
    );
    assert!(e.fix.unwrap().contains(":3000"));
}

#[test]
fn flags_echo_the_short_form_when_the_ports_match() {
    assert_eq!(
        flag(PortMap {
            mac: 3000,
            vm: 3000
        }),
        "-p 3000"
    );
    assert_eq!(flag(PortMap { mac: 8080, vm: 80 }), "-p 8080:80");
}
