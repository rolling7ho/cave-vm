use std::thread;

use cave_vm_proto::agent::{Exec, Signal, Stream};

use super::*;

fn read_msg(stream: &mut UnixStream) -> Option<Message> {
    read_frame(stream).unwrap()
}

fn read_reply(stream: &mut UnixStream) -> Option<Reply> {
    read_frame(stream).unwrap()
}

fn exec(argv: &[&str]) -> Message {
    Message::Exec(Exec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        stdin: true,
        ..Exec::default()
    })
}

/// A fake agent: answers hello, checks the request, then runs `script`.
fn fake_agent(
    mut agent: UnixStream,
    script: impl FnOnce(&mut UnixStream) + Send + 'static,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Some(Message::Hello(hello)) = read_msg(&mut agent) else {
            panic!("no hello");
        };
        let reply = hello.accept(VERSIONS, "fake-agent").unwrap();
        write_frame(&mut agent, &Message::Hello(reply)).unwrap();
        script(&mut agent);
    })
}

#[test]
fn input_goes_up_and_output_and_exit_come_back() {
    let (mut client, mut daemon_side) = UnixStream::pair().unwrap();
    let (daemon_agent, agent) = UnixStream::pair().unwrap();
    let agent = fake_agent(agent, |agent| {
        assert_eq!(read_msg(agent), Some(exec(&["cat"])));
        let Some(Message::Input { data }) = read_msg(agent) else {
            panic!("no input");
        };
        write_frame(
            agent,
            &Message::Output {
                stream: Stream::Stdout,
                data,
            },
        )
        .unwrap();
        assert_eq!(read_msg(agent), Some(Message::InputEnd));
        assert_eq!(
            read_msg(agent),
            Some(Message::Signal {
                signal: Signal::Int
            })
        );
        write_frame(
            agent,
            &Message::Exit {
                code: Some(3),
                signal: None,
            },
        )
        .unwrap();
    });
    let daemon =
        thread::spawn(move || run(&mut daemon_side, daemon_agent, &exec(&["cat"])).unwrap());

    write_frame(
        &mut client,
        &Request::Input {
            data: b"hi\n".to_vec().into(),
        },
    )
    .unwrap();
    assert_eq!(
        read_reply(&mut client),
        Some(Reply::Output {
            stream: Stream::Stdout,
            data: b"hi\n".to_vec().into()
        })
    );
    write_frame(&mut client, &Request::InputEnd).unwrap();
    // Requests that are not stream messages are dropped, not relayed.
    write_frame(&mut client, &Request::Images).unwrap();
    write_frame(
        &mut client,
        &Request::Signal {
            signal: Signal::Int,
        },
    )
    .unwrap();
    assert_eq!(
        read_reply(&mut client),
        Some(Reply::Exit {
            code: Some(3),
            signal: None
        })
    );
    agent.join().unwrap();
    daemon.join().unwrap();
}

#[test]
fn a_client_that_goes_away_closes_the_agent_connection() {
    let (client, mut daemon_side) = UnixStream::pair().unwrap();
    let (daemon_agent, agent) = UnixStream::pair().unwrap();
    let agent = fake_agent(agent, |agent| {
        assert!(read_msg(agent).is_some());
        // The agent kills the program when it sees the end.
        assert_eq!(read_frame::<_, Message>(agent).ok().flatten(), None);
    });
    let daemon = thread::spawn(move || run(&mut daemon_side, daemon_agent, &exec(&["sleep"])));
    drop(client);
    agent.join().unwrap();
    // Writing the reply to the gone client fails; the relay still ends.
    let _ = daemon.join().unwrap();
}

#[test]
fn an_agent_that_closes_mid_command_is_an_error_with_a_fix() {
    let (mut client, mut daemon_side) = UnixStream::pair().unwrap();
    let (daemon_agent, agent) = UnixStream::pair().unwrap();
    let agent = fake_agent(agent, |agent| {
        assert!(read_msg(agent).is_some());
    });
    let daemon = thread::spawn(move || run(&mut daemon_side, daemon_agent, &exec(&["true"])));
    agent.join().unwrap();
    let Some(Reply::Error(e)) = read_reply(&mut client) else {
        panic!("no error");
    };
    assert!(e.message.contains("guest agent"), "{e:?}");
    assert!(e.fix.is_some());
    drop(client);
    daemon.join().unwrap().unwrap();
}
