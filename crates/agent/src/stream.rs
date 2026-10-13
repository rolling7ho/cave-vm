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

/// The writing half of a connection, shared by the threads of one
/// operation so frames never interleave.
#[derive(Clone)]
pub struct Out(Arc<Mutex<Box<dyn Stream>>>);

impl Out {
    pub fn new(stream: Box<dyn Stream>) -> Out {
        Out(Arc::new(Mutex::new(stream)))
    }

    pub fn send(&self, message: &Message) -> io::Result<()> {
        let mut stream = self.0.lock().expect("out lock");
        write_frame(&mut *stream, message).map_err(io::Error::other)
    }

    /// Sends an `error`, which ends the operation.
    pub fn error(&self, code: ErrorCode, message: impl Into<String>) -> io::Result<()> {
        self.send(&Message::Error(ErrorMessage::new(code, message)))
    }

    /// Writes raw bytes, for port forwarding after `port_forward_ok`.
    pub fn write_raw(&self, data: &[u8]) -> io::Result<()> {
        self.0.lock().expect("out lock").write_all(data)
    }

    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.lock().expect("out lock").shutdown_stream(how)
    }
}
