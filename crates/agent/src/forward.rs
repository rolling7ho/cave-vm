//! `port_forward`: connects to a TCP port in the guest, then carries raw
//! bytes both ways.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::thread;

use cave_vm_proto::ErrorCode;
use cave_vm_proto::agent::Message;

use crate::stream::{Out, Stream};

pub fn run(port: u16, mut reader: Box<dyn Stream>, out: Out) -> io::Result<()> {
    let tcp = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(tcp) => tcp,
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            return out.error(
                ErrorCode::NotFound,
                format!("nothing is listening on port {port} in the VM"),
            );
        }
        Err(e) => return out.error(ErrorCode::Io, format!("port {port}: {e}")),
    };
    out.send(&Message::PortForwardOk)?;

    let mut tcp_write = tcp.try_clone()?;
    let to_guest = thread::spawn(move || {
        let _ = io::copy(&mut reader, &mut tcp_write);
        let _ = tcp_write.shutdown(Shutdown::Write);
    });
    let mut tcp_read = tcp;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match tcp_read.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_raw(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = out.shutdown(Shutdown::Write);
    let _ = to_guest.join();
    Ok(())
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_raw(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
