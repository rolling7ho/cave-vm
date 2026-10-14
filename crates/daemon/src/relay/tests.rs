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
    let daemon = thread::spawn(move || run(&mut daemon_side, daemon_agent, &exec(&["cat"])));

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
    let ended = daemon.join().unwrap();
    ended.result.unwrap();
    assert_eq!(
        ended.last,
        Some(Reply::Exit {
            code: Some(3),
            signal: None
        })
    );
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
    // Writing the reply to the gone client fails; the relay still ends,
    // with the agent's hang-up as its last reply.
    let ended = daemon.join().unwrap();
    assert!(ended.result.is_err());
    assert!(matches!(ended.last, Some(Reply::Error(_))), "{ended:?}");
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
    daemon.join().unwrap().result.unwrap();
}

#[test]
fn file_reads_and_writes_end_with_their_last_message() {
    // A read ends at `file_end`.
    let (mut client, mut daemon_side) = UnixStream::pair().unwrap();
    let (daemon_agent, agent) = UnixStream::pair().unwrap();
    let read_request = Message::FileRead {
        path: "/workspace/a".into(),
    };
    let expected = read_request.clone();
    let agent = fake_agent(agent, move |agent| {
        assert_eq!(read_msg(agent), Some(expected));
        write_frame(
            agent,
            &Message::FileInfo {
                size: 2,
                mode: 0o644,
            },
        )
        .unwrap();
        write_frame(
            agent,
            &Message::FileData {
                data: b"hi".to_vec().into(),
            },
        )
        .unwrap();
        write_frame(
            agent,
            &Message::FileEnd {
                sha256: "ab".into(),
            },
        )
        .unwrap();
    });
    let daemon = thread::spawn(move || run(&mut daemon_side, daemon_agent, &read_request));
    assert_eq!(
        read_reply(&mut client),
        Some(Reply::FileInfo {
            size: 2,
            mode: 0o644
        })
    );
    assert!(matches!(
        read_reply(&mut client),
        Some(Reply::FileData { .. })
    ));
    assert_eq!(
        read_reply(&mut client),
        Some(Reply::FileEnd {
            sha256: "ab".into()
        })
    );
    agent.join().unwrap();
    daemon.join().unwrap().result.unwrap();

    // A write relays the data up and ends at `file_written`.
    let (mut client, mut daemon_side) = UnixStream::pair().unwrap();
    let (daemon_agent, agent) = UnixStream::pair().unwrap();
    let write_request = Message::FileWrite {
        path: "/workspace/b".into(),
        mode: Some(0o600),
        create_dirs: true,
    };
    let agent = fake_agent(agent, |agent| {
        assert!(matches!(read_msg(agent), Some(Message::FileWrite { .. })));
        assert!(matches!(read_msg(agent), Some(Message::FileData { .. })));
        assert_eq!(
            read_msg(agent),
            Some(Message::FileEnd {
                sha256: "cd".into()
            })
        );
        write_frame(agent, &Message::FileWritten { size: 3 }).unwrap();
    });
    let daemon = thread::spawn(move || run(&mut daemon_side, daemon_agent, &write_request));
    write_frame(
        &mut client,
        &Request::FileData {
            data: b"abc".to_vec().into(),
        },
    )
    .unwrap();
    write_frame(
        &mut client,
        &Request::FileEnd {
            sha256: "cd".into(),
        },
    )
    .unwrap();
    assert_eq!(
        read_reply(&mut client),
        Some(Reply::FileWritten { size: 3 })
    );
    agent.join().unwrap();
    let ended = daemon.join().unwrap();
    ended.result.unwrap();
    assert_eq!(ended.last, Some(Reply::FileWritten { size: 3 }));
}

#[test]
fn injected_values_are_redacted_even_across_chunks() {
    let (mut client, mut daemon_side) = UnixStream::pair().unwrap();
    let (daemon_agent, agent) = UnixStream::pair().unwrap();
    let agent = fake_agent(agent, |agent| {
        read_msg(agent).unwrap();
        for (stream, text) in [
            (Stream::Stdout, "API_KEY_1: sned"),
            (Stream::Stderr, "err sne"),
            (Stream::Stdout, "nao_#m34nr\nbye "),
            (Stream::Stderr, "d"),
            (Stream::Stdout, "snednao"),
        ] {
            let data = text.as_bytes().to_vec().into();
            write_frame(agent, &Message::Output { stream, data }).unwrap();
        }
        let exit = Message::Exit {
            code: Some(0),
            signal: None,
        };
        write_frame(agent, &exit).unwrap();
    });
    let redactor = Redactor::new(&[("API_KEY_1".into(), "snednao_#m34nr".into())]);
    let daemon = thread::spawn(move || {
        run_redacted(&mut daemon_side, daemon_agent, &exec(&["env"]), &redactor)
    });
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        match read_reply(&mut client).unwrap() {
            Reply::Output { stream, data } => match stream {
                Stream::Stdout => stdout.extend(data.0),
                Stream::Stderr => stderr.extend(data.0),
            },
            Reply::Exit { code, .. } => {
                assert_eq!(code, Some(0));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        String::from_utf8(stdout).unwrap(),
        "API_KEY_1: [API_KEY_1]\nbye snednao"
    );
    assert_eq!(String::from_utf8(stderr).unwrap(), "err sned");
    agent.join().unwrap();
    assert!(daemon.join().unwrap().result.is_ok());
}
