//! Wire transport abstraction.
//!
//! The spacedesk driver listens on TCP 28252 and sniffs the first bytes of the
//! connection: an HTTP `GET ... Upgrade: websocket` request switches it to
//! WebSocket mode (used by the HTML5 viewer), anything else is parsed as raw
//! binary packets (native mode, used by the official Windows/Android viewers).
//! In both modes the application data is the same packet stream:
//! 128-byte header + `payload_len` bytes.

use crate::protocol::{Transport, HEADER_LEN};
use crate::ws::{WsClient, WsError, WsErrorKind, WsMessage};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub enum Conn {
    Ws(WsClient),
    Tcp(TcpConn),
}

/// Raw TCP connection with a buffered packet reader (mirrors WsClient's buffer).
pub struct TcpConn {
    stream: TcpStream,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl TcpConn {
    fn fill(&mut self, want: usize) -> Result<(), WsError> {
        while self.read_buf.len() - self.read_pos < want {
            if self.read_pos > 0 {
                self.read_buf.drain(..self.read_pos);
                self.read_pos = 0;
            }
            let mut tmp = [0u8; 16384];
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    return Err(WsError {
                        kind: WsErrorKind::Closed,
                        detail: "peer closed connection (EOF)".to_string(),
                    })
                }
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Err(WsError {
                        kind: WsErrorKind::Timeout,
                        detail: e.to_string(),
                    })
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let out = self.read_buf[self.read_pos..self.read_pos + n].to_vec();
        self.read_pos += n;
        out
    }
}

impl Conn {
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        host_header: &str,
        transport: Transport,
        timeout: Duration,
    ) -> Result<Conn, WsError> {
        match transport {
            Transport::Ws => Ok(Conn::Ws(WsClient::connect(addr, host_header, timeout)?)),
            Transport::Tcp => {
                let stream = TcpStream::connect(addr)?;
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                stream.set_nodelay(true)?;
                Ok(Conn::Tcp(TcpConn {
                    stream,
                    read_buf: Vec::with_capacity(64 * 1024),
                    read_pos: 0,
                }))
            }
        }
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        match self {
            Conn::Ws(w) => w.set_read_timeout(t),
            Conn::Tcp(c) => c.stream.set_read_timeout(t),
        }
    }

    /// Send one packet (raw bytes on TCP, one binary WS message on WS).
    pub fn send_packet(&mut self, data: &[u8]) -> Result<(), WsError> {
        match self {
            Conn::Ws(w) => w.send_binary(data),
            Conn::Tcp(c) => {
                c.stream.write_all(data)?;
                c.stream.flush()?;
                Ok(())
            }
        }
    }

    /// Read one protocol message: a WS binary message, or one raw packet
    /// (128-byte header + payload). Returns `Ok(None)` on clean close.
    pub fn read_message(&mut self) -> Result<Option<Vec<u8>>, WsError> {
        match self {
            Conn::Ws(w) => match w.read_message()? {
                Some(WsMessage::Binary(b)) => Ok(Some(b)),
                Some(WsMessage::Text(_)) => Ok(Some(Vec::new())),
                None => Ok(None),
            },
            Conn::Tcp(c) => {
                c.fill(HEADER_LEN)?;
                let mut header = [0u8; HEADER_LEN];
                header.copy_from_slice(&c.read_buf[c.read_pos..c.read_pos + HEADER_LEN]);
                let len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                c.fill(HEADER_LEN + len)?;
                let mut msg = c.take(HEADER_LEN);
                msg.extend_from_slice(&c.take(len));
                Ok(Some(msg))
            }
        }
    }

    pub fn close(&mut self, code: u16) -> Result<(), WsError> {
        match self {
            Conn::Ws(w) => w.send_close(code),
            Conn::Tcp(c) => {
                let _ = c.stream.shutdown(Shutdown::Both);
                Ok(())
            }
        }
    }

    pub fn last_close(&self) -> Option<(u16, String)> {
        match self {
            Conn::Ws(w) => w.last_close.clone(),
            Conn::Tcp(_) => Some((1006, "tcp connection closed".to_string())),
        }
    }
}
