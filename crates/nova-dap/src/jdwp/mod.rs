use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum JdwpError {
    #[error("JDWP client is not connected")]
    NotConnected,
    #[error("JDWP operation not implemented")]
    NotImplemented,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("JDWP handshake failed")]
    HandshakeFailed,
}

#[derive(Debug, Clone)]
pub struct ThreadInfo {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct StackFrameInfo {
    pub id: u64,
    pub name: String,
    pub source_path: Option<String>,
    pub line: u32,
}

/// Minimal, mock-friendly interface for the Java Debug Wire Protocol.
///
/// The implementation included in this repository purposefully keeps the API
/// small; it is expected to grow as Nova's debugger matures.
pub trait JdwpClient: Send {
    fn connect(&mut self, host: &str, port: u16) -> Result<(), JdwpError>;

    fn set_line_breakpoint(
        &mut self,
        class: &str,
        method: Option<&str>,
        line: u32,
    ) -> Result<(), JdwpError>;

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, JdwpError>;
    fn stack_frames(&mut self, thread_id: u64) -> Result<Vec<StackFrameInfo>, JdwpError>;

    fn r#continue(&mut self, thread_id: u64) -> Result<(), JdwpError>;
    fn next(&mut self, thread_id: u64) -> Result<(), JdwpError>;
    fn step_in(&mut self, thread_id: u64) -> Result<(), JdwpError>;
    fn step_out(&mut self, thread_id: u64) -> Result<(), JdwpError>;
    fn pause(&mut self, thread_id: u64) -> Result<(), JdwpError>;

    fn evaluate(&mut self, _expression: &str, _frame_id: u64) -> Result<String, JdwpError> {
        Err(JdwpError::NotImplemented)
    }
}

/// A very small JDWP client.
///
/// Currently this implements only the initial JDWP handshake. Higher-level
/// commands (setting breakpoints, querying threads, etc.) are stubbed behind
/// [`JdwpError::NotImplemented`] while the wire protocol is filled out.
pub struct TcpJdwpClient {
    stream: Option<TcpStream>,
}

impl TcpJdwpClient {
    pub fn new() -> Self {
        Self { stream: None }
    }

    fn stream_mut(&mut self) -> Result<&mut TcpStream, JdwpError> {
        self.stream.as_mut().ok_or(JdwpError::NotConnected)
    }

    fn perform_handshake(stream: &mut TcpStream) -> Result<(), JdwpError> {
        const HANDSHAKE: &[u8] = b"JDWP-Handshake";

        stream.write_all(HANDSHAKE)?;
        stream.flush()?;

        let mut reply = [0u8; HANDSHAKE.len()];
        stream.read_exact(&mut reply)?;
        if reply != HANDSHAKE {
            return Err(JdwpError::HandshakeFailed);
        }
        Ok(())
    }
}

impl Default for TcpJdwpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl JdwpClient for TcpJdwpClient {
    fn connect(&mut self, host: &str, port: u16) -> Result<(), JdwpError> {
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unable to resolve JDWP address"))?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        Self::perform_handshake(&mut stream)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn set_line_breakpoint(
        &mut self,
        _class: &str,
        _method: Option<&str>,
        _line: u32,
    ) -> Result<(), JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn threads(&mut self) -> Result<Vec<ThreadInfo>, JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn stack_frames(&mut self, _thread_id: u64) -> Result<Vec<StackFrameInfo>, JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn r#continue(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn next(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn step_in(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn step_out(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }

    fn pause(&mut self, _thread_id: u64) -> Result<(), JdwpError> {
        let _ = self.stream_mut()?;
        Err(JdwpError::NotImplemented)
    }
}
