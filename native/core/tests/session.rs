//! End-to-end session test: a mock spacedesk server on loopback validates the
//! identification handshake, streams visibility / fragmented raw frames / JPEG
//! frames / cursor packets, and validates the client's ACKs and KVM packets.

use spacedesk_core::protocol::Transport;
use spacedesk_core::protocol::*;
use spacedesk_core::session::{run, ClientCommand, ClientConfig, ServerEvent};
use spacedesk_core::ws::WsMessage;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// --- minimal server-side WebSocket helpers (mirrors tests/ws.rs) -------------

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
        assert!(n > 0, "client closed unexpectedly");
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn server_send_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mut frame = vec![0x80 | opcode];
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
    stream
        .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();
}

fn server_packet(ty: HeaderType, payload_len: u32) -> Vec<u8> {
    let mut p = vec![0u8; HEADER_LEN];
    put_u32(&mut p, 0, ty as u32);
    put_u32(&mut p, 4, payload_len);
    p
}

fn frame_packet(
    width: u32,
    height: u32,
    format: u32,
    compression: u32,
    fragment: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = server_packet(HeaderType::FrameBuffer, payload.len() as u32);
    put_u32(&mut p, 8, width);
    put_u32(&mut p, 12, height);
    put_u32(&mut p, 16, width * 4);
    put_u32(&mut p, 20, format);
    put_u32(&mut p, 24, 0);
    put_u32(&mut p, 28, 0);
    put_u32(&mut p, 32, width);
    put_u32(&mut p, 36, height);
    put_u32(&mut p, 40, compression);
    put_u32(&mut p, 64, fragment);
    p.extend_from_slice(payload);
    p
}

/// Wait for a server event with timeout.
fn recv_event(rx: &mpsc::Receiver<ServerEvent>, ms: u64) -> ServerEvent {
    rx.recv_timeout(Duration::from_millis(ms))
        .expect("timed out waiting for server event")
}

#[test]
fn session_handshake_frames_acks_and_kvm() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        server_handshake(&mut stream);
        let mut buf = Vec::new();

        // 1) client must send the identification packet first
        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(payload.len(), PACKET_LEN);
        let (packets, rest) = parse_packets(&payload);
        assert!(rest.is_empty());
        assert_eq!(packets.len(), 1);
        // validate raw identification fields
        assert_eq!(get_u32(&payload, 0), 0);
        assert_eq!(get_u32(&payload, 4), PAYLOAD_LEN as u32);
        assert_eq!(get_u32(&payload, 8), VERSION_MAJOR);
        assert_eq!(get_u32(&payload, 12), VERSION_MINOR);
        assert_eq!(get_u32(&payload, 24), Compression::H264 as u32);

        // 2) send visibility
        let mut v = server_packet(HeaderType::Visibility, 0);
        put_u32(&mut v, 8, 1);
        server_send_frame(&mut stream, 0x2, &v);

        // 3) send a raw 4x4 BGRA frame split into 4 fragments (16 bytes each)
        let raw: Vec<u8> = (0..64u8).collect();
        for i in 0..4 {
            let chunk = &raw[i * 16..(i + 1) * 16];
            let f = frame_packet(4, 4, 4, 0, 1, chunk);
            server_send_frame(&mut stream, 0x2, &f);
        }
        // client must ACK exactly once (after the 4th fragment)
        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(get_u32(&payload, 0), HeaderType::FlowControlAck as u32);
        assert_eq!(payload.len(), HEADER_LEN);

        // 4) send a self-contained JPEG frame (SOI..EOI) → immediate frame + ACK
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend_from_slice(&[0x11; 30]);
        jpeg.extend_from_slice(&[0xFF, 0xD9]);
        let f = frame_packet(4, 4, 4, 3, 0, &jpeg);
        server_send_frame(&mut stream, 0x2, &f);
        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(get_u32(&payload, 0), HeaderType::FlowControlAck as u32);

        // 5) send cursor position
        let mut c = server_packet(HeaderType::CursorPosition, 0);
        put_u32(&mut c, 8, 42);
        put_u32(&mut c, 12, 24);
        put_u32(&mut c, 16, 1);
        server_send_frame(&mut stream, 0x2, &c);

        // 6) expect KVM packets from client commands
        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(get_u32(&payload, 0), HeaderType::Mouse as u32);
        assert_eq!(get_u32(&payload, 8) as i32, 3);
        assert_eq!(get_u32(&payload, 12) as i32, -2);
        assert_eq!(get_u32(&payload, 20), mouse_events::MOVE);

        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(get_u32(&payload, 0), HeaderType::Mouse as u32);
        assert_eq!(get_u32(&payload, 20), mouse_events::LEFT_DOWN);

        let (op, payload) = server_read_frame(&mut stream, &mut buf);
        assert_eq!(op, 0x2);
        assert_eq!(get_u32(&payload, 0), HeaderType::Keyboard as u32);
        assert_eq!(get_u32(&payload, 8), 0x41);
        assert_eq!(get_u32(&payload, 16), key_events::SCAN_CODE);

        // 7) server disconnects
        server_send_frame(&mut stream, 0x8, &1000u16.to_be_bytes());
    });

    let mut cfg = ClientConfig {
        host: "127.0.0.1".to_string(),
        port,
        reconnect: false,
        ..Default::default()
    };
    cfg.identification.transport = Transport::Ws;
    cfg.identification.compression = Compression::H264 as u32;

    let (ev_tx, ev_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();

    let session_thread = std::thread::spawn(move || run(&cfg, ev_tx, cmd_rx));

    // client side assertions
    assert!(matches!(recv_event(&ev_rx, 5000), ServerEvent::Connected));
    assert!(matches!(
        recv_event(&ev_rx, 5000),
        ServerEvent::Visibility(true)
    ));
    match recv_event(&ev_rx, 5000) {
        ServerEvent::Frame(p) => {
            assert_eq!(p.compression, 0);
            assert_eq!(p.width, 4);
            assert_eq!(p.height, 4);
            assert_eq!(p.payload, (0..64u8).collect::<Vec<u8>>());
        }
        other => panic!("expected frame, got {:?}", other),
    }
    match recv_event(&ev_rx, 5000) {
        ServerEvent::Frame(p) => {
            assert_eq!(p.compression, 3);
            assert_eq!(p.payload[0], 0xFF);
            assert_eq!(p.payload[1], 0xD8);
        }
        other => panic!("expected jpeg frame, got {:?}", other),
    }
    match recv_event(&ev_rx, 5000) {
        ServerEvent::CursorPosition { x, y, visible } => {
            assert_eq!((x, y, visible), (42, 24, true));
        }
        other => panic!("expected cursor position, got {:?}", other),
    }

    cmd_tx
        .send(ClientCommand::MouseMove { dx: 3, dy: -2 })
        .unwrap();
    cmd_tx
        .send(ClientCommand::MouseButton {
            button: MouseButton::Left,
            down: true,
        })
        .unwrap();
    cmd_tx
        .send(ClientCommand::Key {
            vk: 0x41,
            down: true,
            extended: false,
        })
        .unwrap();

    // wait for server close → session ends with Disconnected
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match recv_event(&ev_rx, 10_000) {
            ServerEvent::Disconnected { .. } => break,
            ServerEvent::Error(e) => panic!("unexpected error: {}", e),
            other => {
                if Instant::now() > deadline {
                    panic!("timeout, last event: {:?}", other);
                }
            }
        }
    }
    session_thread.join().unwrap().unwrap();
    server.join().unwrap();
}

#[test]
fn raw_decoder_passthrough() {
    use spacedesk_core::decoder::{Decoder, RawDecoder};
    use spacedesk_core::frame::PixelFormat;
    let payload: Vec<u8> = (0..4 * 4 * 4u8).collect();
    let pkt = FrameBufferPacket {
        payload_len: payload.len() as u32,
        width: 4,
        height: 4,
        pitch: 16,
        format: 4,
        rect: Rect {
            x: 0,
            y: 0,
            x2: 4,
            y2: 4,
        },
        compression: 0,
        fragment_info: 0,
        payload: payload.clone(),
    };
    let frames = RawDecoder.submit(&pkt).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].format, PixelFormat::Bgra);
    assert_eq!(frames[0].data, payload);
    assert_eq!(
        frames[0].rect,
        Rect {
            x: 0,
            y: 0,
            x2: 4,
            y2: 4
        }
    );
    let _ = WsMessage::Binary(vec![]); // silence unused import in some cfgs
}

// ---------------------------------------------------------------------------
// Raw TCP (native) transport test: no HTTP/WS handshake, packets are raw.
// ---------------------------------------------------------------------------

fn tcp_read_packet(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Vec<u8> {
    loop {
        if buf.len() >= HEADER_LEN {
            let plen = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            if buf.len() >= HEADER_LEN + plen {
                let pkt = buf[..HEADER_LEN + plen].to_vec();
                buf.drain(..HEADER_LEN + plen);
                return pkt;
            }
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).unwrap();
        assert!(n > 0, "server closed");
        buf.extend_from_slice(&tmp[..n]);
    }
}

#[test]
fn session_raw_tcp_native_handshake_ping_echo_ack() {
    use std::io::Write;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buf = Vec::new();

        // 1) raw identification packet, no HTTP handshake
        let ident = tcp_read_packet(&mut stream, &mut buf);
        assert_eq!(ident.len(), PACKET_LEN);
        assert_eq!(get_u32(&ident, 0), 0); // Identification
        assert_eq!(get_u32(&ident, 4), PAYLOAD_LEN as u32);
        assert_eq!(get_u32(&ident, 8), VERSION_MAJOR);
        assert_eq!(get_u32(&ident, 12), VERSION_MINOR);
        assert_eq!(get_u32(&ident, 16), 0); // WindowsRemoteMonitor
        assert_eq!(get_u32(&ident, 20), 3); // native layout
        assert_eq!(get_u32(&ident, 28), 2);
        assert_eq!(get_u32(&ident, 36), 0x10003);
        assert_eq!(get_u16(&ident, 44), 30); // real fps
        assert_eq!(get_u16(&ident, 46), 4);
        let payload = &ident[HEADER_LEN..];
        assert_eq!(get_u32(payload, 0), 1); // native payload magic

        // 2) send a Ping (type 1) with payload; client must echo it back
        let mut ping = vec![0u8; HEADER_LEN + 4];
        put_u32(&mut ping, 0, 1);
        put_u32(&mut ping, 4, 4);
        ping[HEADER_LEN..].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        stream.write_all(&ping).unwrap();
        let echo = tcp_read_packet(&mut stream, &mut buf);
        assert_eq!(get_u32(&echo, 0), 1, "expected echoed ping");
        assert_eq!(&echo[HEADER_LEN..], &[0xDE, 0xAD, 0xBE, 0xEF]);

        // 3) send a full-screen raw frame (type 2, 4x4, BGRA, non-fragmented)
        let raw: Vec<u8> = (0..64u8).collect();
        let f = frame_packet(4, 4, 4, 0, 0, &raw);
        stream.write_all(&f).unwrap();
        // expect a FlowControlAck
        let ack = tcp_read_packet(&mut stream, &mut buf);
        assert_eq!(get_u32(&ack, 0), HeaderType::FlowControlAck as u32);

        // 4) expect a KVM mouse packet
        let mv = tcp_read_packet(&mut stream, &mut buf);
        assert_eq!(get_u32(&mv, 0), HeaderType::Mouse as u32);
        assert_eq!(get_u32(&mv, 20), mouse_events::MOVE);
    });

    let mut cfg = ClientConfig {
        host: "127.0.0.1".to_string(),
        port,
        reconnect: false,
        ..Default::default()
    };
    cfg.identification.transport = Transport::Tcp;
    cfg.identification.client_type = 0;
    cfg.identification.compression = Compression::H264 as u32;

    let (ev_tx, ev_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let session_thread = std::thread::spawn(move || run(&cfg, ev_tx, cmd_rx));

    assert!(matches!(recv_event(&ev_rx, 5000), ServerEvent::Connected));
    match recv_event(&ev_rx, 5000) {
        ServerEvent::Frame(p) => {
            assert_eq!(p.payload, (0..64u8).collect::<Vec<u8>>());
            assert_eq!(p.tile_dims(), (4, 4));
        }
        other => panic!("expected frame, got {:?}", other),
    }
    cmd_tx
        .send(ClientCommand::MouseMove { dx: 5, dy: 6 })
        .unwrap();
    // wait for server thread to finish reading our packets
    server.join().unwrap();
    let _ = session_thread.join();
}
