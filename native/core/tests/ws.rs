//! WebSocket layer tests against an in-process mock server (loopback TCP).

use spacedesk_core::ws::{WsClient, WsMessage};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Read one client (masked) frame from the server side; returns (opcode, payload).
fn server_read_frame(stream: &mut TcpStream, buf: &mut Vec<u8>) -> (u8, Vec<u8>) {
    loop {
        if buf.len() >= 2 {
            let b1 = buf[1];
            let mut len = (b1 & 0x7F) as usize;
            let mut hlen = 2usize;
            if len == 126 {
                if buf.len() >= 4 {
                    len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                    hlen = 4;
                } else {
                    hlen = 0;
                }
            } else if len == 127 {
                if buf.len() >= 10 {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&buf[2..10]);
                    len = u64::from_be_bytes(b) as usize;
                    hlen = 10;
                } else {
                    hlen = 0;
                }
            }
            if hlen > 0 && (b1 & 0x80) != 0 {
                hlen += 4;
            }
            if hlen > 0 && buf.len() >= hlen + len {
                let opcode = buf[0] & 0x0F;
                let masked = (b1 & 0x80) != 0;
                let mask = if masked {
                    Some([buf[hlen - 4], buf[hlen - 3], buf[hlen - 2], buf[hlen - 1]])
                } else {
                    None
                };
                let mut payload = buf[hlen..hlen + len].to_vec();
                if let Some(m) = mask {
                    for (i, b) in payload.iter_mut().enumerate() {
                        *b ^= m[i % 4];
                    }
                }
                buf.drain(..hlen + len);
                return (opcode, payload);
            }
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).unwrap();
        assert!(n > 0, "client closed");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn server_send_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    server_send_frame_fin(stream, true, opcode, payload)
}

fn server_send_frame_fin(stream: &mut TcpStream, fin: bool, opcode: u8, payload: &[u8]) {
    let mut frame = vec![if fin { 0x80 | opcode } else { opcode }];
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn server_handshake(stream: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).unwrap();
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let req = String::from_utf8_lossy(&buf);
    assert!(req.starts_with("GET / HTTP/1.1"), "bad request: {}", req);
    assert!(req.contains("Upgrade: websocket"));
    assert!(req.contains("Sec-WebSocket-Version: 13"));
    stream
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
        )
        .unwrap();
    stream.flush().unwrap();
}

#[test]
fn ws_handshake_fragmented_message_and_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        server_handshake(&mut stream);
        let mut buf = Vec::new();
        // send a binary message split into 2 continuation frames, with a ping in between
        server_send_frame_fin(&mut stream, false, 0x2, b"hello "); // FIN=0 binary
        server_send_frame(&mut stream, 0x9, b"pingdata"); // ping
        server_send_frame_fin(&mut stream, true, 0x0, b"world"); // FIN=1 continuation
        // expect a pong back
        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0xA);
        assert_eq!(payload, b"pingdata");
        // now a big message from the client (>64KiB → 127 length path)
        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(payload.len(), 200_000);
        assert!(payload.iter().all(|b| *b == 0xAB));
        server_send_frame(&mut stream, 0x8, &1000u16.to_be_bytes()); // close
    });

    let mut client = WsClient::connect(
        ("127.0.0.1", port),
        &format!("127.0.0.1:{}", port),
        Duration::from_secs(5),
    )
    .unwrap();
    client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    match client.read_message().unwrap() {
        Some(WsMessage::Binary(data)) => assert_eq!(data, b"hello world"),
        other => panic!("unexpected {:?}", other),
    }
    client.send_binary(&vec![0xAB; 200_000]).unwrap();
    assert!(client.read_message().unwrap().is_none()); // clean close
    server.join().unwrap();
}

#[test]
fn ws_handshake_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut tmp = [0u8; 1024];
        let _ = stream.read(&mut tmp).unwrap();
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    });
    let res = WsClient::connect(("127.0.0.1", port), "x", Duration::from_secs(5));
    assert!(res.is_err());
    server.join().unwrap();
}
