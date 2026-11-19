//! Minimal RFC 6455 WebSocket client (binary messages only, no TLS — spacedesk
//! servers on the LAN use plain `ws://`).

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

#[derive(Debug)]
pub enum WsMessage {
    Binary(Vec<u8>),
    Text(String),
}

#[derive(Debug)]
pub struct WsError {
    pub kind: WsErrorKind,
    pub detail: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WsErrorKind {
    Io,
    /// Read timed out (only when a read timeout is set); not a real error.
    Timeout,
    Handshake,
    Closed,
    Protocol,
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for WsError {}

impl From<io::Error> for WsError {
    fn from(e: io::Error) -> Self {
        WsError {
            kind: WsErrorKind::Io,
            detail: e.to_string(),
        }
    }
}

fn b64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TBL[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TBL[n as usize & 63] as char } else { '=' });
    }
    out
}

fn pseudo_random_bytes(n: usize) -> Vec<u8> {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        ^ (std::process::id() as u64) << 32;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push((seed & 0xFF) as u8);
    }
    out
}

pub struct WsClient {
    stream: TcpStream,
    read_buf: Vec<u8>,
    read_pos: usize,
    closed: bool,
}

impl WsClient {
    /// TCP connect + WebSocket opening handshake against `ws://host:port/`.
    pub fn connect<A: ToSocketAddrs>(
        addr: A,
        host_header: &str,
        timeout: Duration,
    ) -> Result<WsClient, WsError> {
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let mut client = WsClient {
            stream,
            read_buf: Vec::with_capacity(64 * 1024),
            read_pos: 0,
            closed: false,
        };
        client.handshake(host_header)?;
        // after the handshake we want non-blocking-ish behaviour handled by the caller
        // through read timeouts; keep the timeout set by the caller afterwards.
        Ok(client)
    }

    pub fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(t)
    }

    fn handshake(&mut self, host_header: &str) -> Result<(), WsError> {
        let key = b64_encode(&pseudo_random_bytes(16));
        let request = format!(
            "GET / HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n",
            host_header, key
        );
        self.stream.write_all(request.as_bytes())?;
        self.stream.flush()?;

        // read until end of HTTP headers
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pos) = find_subsequence(&self.read_buf[self.read_pos..], b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&self.read_buf[self.read_pos..self.read_pos + pos]).to_string();
                self.read_pos += pos + 4;
                let status = headers.lines().next().unwrap_or("");
                if !status.contains(" 101") {
                    return Err(WsError {
                        kind: WsErrorKind::Handshake,
                        detail: format!("unexpected status line: {}", status),
                    });
                }
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(WsError {
                    kind: WsErrorKind::Handshake,
                    detail: "timeout waiting for handshake response".to_string(),
                });
            }
            let mut tmp = [0u8; 4096];
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    return Err(WsError {
                        kind: WsErrorKind::Handshake,
                        detail: "connection closed during handshake".to_string(),
                    })
                }
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(e) => return Err(e.into()),
            }
        }
    }

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
                        detail: "peer closed connection".to_string(),
                    })
                }
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
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

    /// Read one complete data message. Control frames (ping/pong/close) are handled
    /// internally. Returns `Ok(None)` on a clean close.
    pub fn read_message(&mut self) -> Result<Option<WsMessage>, WsError> {
        let mut message: Option<(u8, Vec<u8>)> = None;
        loop {
            self.fill(2)?;
            let b0 = self.read_buf[self.read_pos];
            let b1 = self.read_buf[self.read_pos + 1];
            let fin = b0 & 0x80 != 0;
            let opcode = b0 & 0x0F;
            let masked = b1 & 0x80 != 0;
            let mut len = (b1 & 0x7F) as u64;
            let mut hlen = 2usize;
            if len == 126 {
                hlen = 4;
            } else if len == 127 {
                hlen = 10;
            }
            if masked {
                hlen += 4;
            }
            self.fill(hlen)?;
            if len == 126 {
                len = u16::from_be_bytes([
                    self.read_buf[self.read_pos + 2],
                    self.read_buf[self.read_pos + 3],
                ]) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                b.copy_from_slice(&self.read_buf[self.read_pos + 2..self.read_pos + 10]);
                len = u64::from_be_bytes(b);
            }
            let len = len as usize;
            self.fill(hlen + len)?;
            let header = self.take(hlen);
            let mask = if masked {
                Some([
                    header[hlen - 4],
                    header[hlen - 3],
                    header[hlen - 2],
                    header[hlen - 1],
                ])
            } else {
                None
            };
            let mut payload = self.take(len);
            if let Some(m) = mask {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= m[i % 4];
                }
            }

            match opcode {
                OP_BINARY | OP_TEXT | OP_CONT => match message.take() {
                    None => {
                        if fin {
                            return Ok(Some(if opcode == OP_BINARY {
                                WsMessage::Binary(payload)
                            } else {
                                WsMessage::Text(String::from_utf8_lossy(&payload).to_string())
                            }));
                        }
                        message = Some((opcode, payload));
                    }
                    Some((op, mut acc)) => {
                        acc.extend_from_slice(&payload);
                        if fin {
                            return Ok(Some(if op == OP_BINARY {
                                WsMessage::Binary(acc)
                            } else {
                                WsMessage::Text(String::from_utf8_lossy(&acc).to_string())
                            }));
                        }
                        message = Some((op, acc));
                    }
                },
                OP_PING => {
                    self.send_frame(OP_PONG, &payload)?;
                }
                OP_PONG => {}
                OP_CLOSE => {
                    self.closed = true;
                    let _ = self.send_frame(OP_CLOSE, &payload);
                    return Ok(None);
                }
                _ => {
                    return Err(WsError {
                        kind: WsErrorKind::Protocol,
                        detail: format!("unsupported opcode {}", opcode),
                    })
                }
            }
        }
    }

    pub fn send_binary(&mut self, data: &[u8]) -> Result<(), WsError> {
        self.send_frame(OP_BINARY, data)
    }

    pub fn send_close(&mut self, code: u16) -> Result<(), WsError> {
        let payload = code.to_be_bytes();
        self.send_frame(OP_CLOSE, &payload)?;
        self.closed = true;
        Ok(())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WsError> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode); // FIN + opcode
        let mask = pseudo_random_bytes(4);
        let len = payload.len();
        if len < 126 {
            frame.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        self.stream.write_all(&frame)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
