//! The connection an operation runs on, and the shared writer for frames.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use cave_vm_proto::agent::Message;
use cave_vm_proto::{ErrorCode, ErrorMessage, write_frame};

/// A byte stream that can be cloned for a second thread and half-closed:
/// vsock on Linux, Unix sockets in tests.
pub trait Stream: Read + Write + Send + 'static {
    fn try_clone_stream(&self) -> io::Result<Box<dyn Stream>>;
    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()>;
}

impl Stream for UnixStream {
    fn try_clone_stream(&self) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()> {
        self.shutdown(how)
    }
}

impl Stream for TcpStream {
    fn try_clone_stream(&self) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()> {
        self.shutdown(how)
    }
}

#[cfg(target_os = "linux")]
impl Stream for crate::sys::Vsock {
    fn try_clone_stream(&self) -> io::Result<Box<dyn Stream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shutdown_stream(&self, how: Shutdown) -> io::Result<()> {
        self.shutdown(how)
    }
}

/// The writing half of a connection, shared by the threads of one
/// operation so frames never interleave.
#[derive(Clone)]
pub struct Out(Arc<Mutex<Writer>>);

struct Writer {
    stream: Box<dyn Stream>,
    /// Set once the operation's last message is sent; later frames from
    /// threads still running are dropped.
    finished: bool,
}

impl Out {
    pub fn new(stream: Box<dyn Stream>) -> Out {
        Out(Arc::new(Mutex::new(Writer {
            stream,
            finished: false,
        })))
    }

    pub fn send(&self, message: &Message) -> io::Result<()> {
        let mut writer = self.0.lock().expect("out lock");
        if writer.finished {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        write_frame(&mut writer.stream, message).map_err(io::Error::other)
    }

    /// Sends the operation's last message; nothing is sent after it.
    pub fn finish(&self, message: &Message) -> io::Result<()> {
        let result = self.send(message);
        self.0.lock().expect("out lock").finished = true;
        result
    }

    /// Sends an `error`, which ends the operation.
    pub fn error(&self, code: ErrorCode, message: impl Into<String>) -> io::Result<()> {
        self.send(&Message::Error(ErrorMessage::new(code, message)))
    }

    /// Writes raw bytes, for port forwarding after `port_forward_ok`.
    pub fn write_raw(&self, data: &[u8]) -> io::Result<()> {
        self.0.lock().expect("out lock").stream.write_all(data)
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.lock().expect("out lock").stream.shutdown_stream(how)
    }
}
