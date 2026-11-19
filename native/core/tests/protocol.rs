//! Byte-exact tests for the spacedesk protocol builders, checked against the
//! layouts of the official HTML5 viewer (src/spacedesk.min.js).

use spacedesk_core::protocol::*;

fn u32at(buf: &[u8], off: usize) -> u32 {
    get_u32(buf, off)
}

#[test]
fn identification_packet_layout() {
    let ident = Identification {
        client_type: ClientType::WindowsRemoteMonitor as u32,
        compression: Compression::H264 as u32,
        quality: 70,
        subsampling: 2,
        frame_rate: 1,
        resolution: (2560, 1440),
        resolution_custom: Some((1920, 1080)),
        client_name: "Test Viewer".to_string(),
        ..Default::default()
    };
    let pkt = identification_packet(&ident);
    assert_eq!(pkt.len(), PACKET_LEN);
    // header
    assert_eq!(u32at(&pkt, 0), 0); // Identification
    assert_eq!(u32at(&pkt, 4), PAYLOAD_LEN as u32);
    assert_eq!(u32at(&pkt, 8), VERSION_MAJOR);
    assert_eq!(u32at(&pkt, 12), VERSION_MINOR);
    assert_eq!(u32at(&pkt, 16), 0); // WindowsRemoteMonitor
    assert_eq!(u32at(&pkt, 20), 2); // subsampling 4:2:0
    assert_eq!(u32at(&pkt, 24), 1); // H264
    assert_eq!(u32at(&pkt, 32), 70); // quality
    assert_eq!(get_u16(&pkt, 44), 1); // frame rate mode
    assert_eq!(u32at(&pkt, 48), 2); // custom resolution mode
    assert_eq!(u32at(&pkt, 52), 2560);
    assert_eq!(u32at(&pkt, 56), 1920);
    assert_eq!(u32at(&pkt, 88), 1440);
    assert_eq!(u32at(&pkt, 92), 1080);
    // payload magic: UTF-16LE "11111" then "22222"
    let payload = &pkt[HEADER_LEN..];
    for i in 0..5 {
        assert_eq!(payload[i * 2], 0x31);
        assert_eq!(payload[i * 2 + 1], 0);
        assert_eq!(payload[10 + i * 2], 0x32);
        assert_eq!(payload[10 + i * 2 + 1], 0);
    }
    // name at offset 78, UTF-16LE
    let name = "Test Viewer";
    for (i, ch) in name.encode_utf16().enumerate() {
        assert_eq!(get_u16(payload, 78 + i * 2), ch);
    }
}

#[test]
fn identification_client_resolution_mode() {
    let ident = Identification {
        resolution: (1920, 1080),
        resolution_custom: None,
        ..Default::default()
    };
    let pkt = identification_packet(&ident);
    assert_eq!(u32at(&pkt, 48), 1);
    assert_eq!(u32at(&pkt, 52), 1920);
    assert_eq!(u32at(&pkt, 56), 0);
    assert_eq!(u32at(&pkt, 88), 1080);
    assert_eq!(u32at(&pkt, 92), 0);
}

#[test]
fn flow_control_ack_layout() {
    let ack = flow_control_ack();
    assert_eq!(ack.len(), HEADER_LEN);
    assert_eq!(u32at(&ack, 0), 7);
    assert_eq!(u32at(&ack, 4), 0);
    assert!(ack[8..].iter().all(|b| *b == 0));
}

#[test]
fn mouse_move_packet_layout() {
    let p = mouse_move_packet(-5, 7);
    assert_eq!(p.len(), HEADER_LEN);
    assert_eq!(u32at(&p, 0), 10);
    assert_eq!(u32at(&p, 4), 0);
    assert_eq!(get_u32(&p, 8) as i32, -5);
    assert_eq!(get_u32(&p, 12) as i32, 7);
    assert_eq!(u32at(&p, 20), mouse_events::MOVE);
}

#[test]
fn mouse_button_packets() {
    let p = mouse_button_packet(MouseButton::Left, true);
    assert_eq!(u32at(&p, 20), mouse_events::LEFT_DOWN);
    let p = mouse_button_packet(MouseButton::Left, false);
    assert_eq!(u32at(&p, 20), mouse_events::LEFT_UP);
    let p = mouse_button_packet(MouseButton::Right, true);
    assert_eq!(u32at(&p, 20), mouse_events::RIGHT_DOWN);
    let p = mouse_button_packet(MouseButton::Middle, false);
    assert_eq!(u32at(&p, 20), mouse_events::MIDDLE_UP);
}

#[test]
fn mouse_wheel_packet_layout() {
    let p = mouse_wheel_packet(-120);
    assert_eq!(u32at(&p, 0), 10);
    assert_eq!(get_u32(&p, 16) as i32, -120);
    assert_eq!(u32at(&p, 20), mouse_events::WHEEL);
}

#[test]
fn keyboard_packet_layout() {
    let down = keyboard_packet(0x41, true, false); // 'A' down
    assert_eq!(down.len(), HEADER_LEN);
    assert_eq!(u32at(&down, 0), 11);
    assert_eq!(u32at(&down, 8), 0x41);
    assert_eq!(u32at(&down, 12), 0x41);
    assert_eq!(u32at(&down, 16), key_events::SCAN_CODE);
    let up = keyboard_packet(0x1B, false, false); // ESC up
    assert_eq!(u32at(&up, 16), key_events::SCAN_CODE | key_events::KEY_UP);
    let ext = keyboard_packet(0x25, true, true); // LEFT arrow down, extended
    assert_eq!(
        u32at(&ext, 16),
        key_events::SCAN_CODE | key_events::EXTEND_KEY
    );
}

#[test]
fn disconnect_packet_layout() {
    let p = disconnect_packet();
    assert_eq!(p.len(), KVM_HEADER_LEN);
    assert_eq!(u32at(&p, 0), 8);
}

#[test]
fn parse_frame_buffer_header() {
    let mut buf = vec![0u8; HEADER_LEN + 64];
    put_u32(&mut buf, 0, 2);
    put_u32(&mut buf, 4, 64);
    put_u32(&mut buf, 8, 1920);
    put_u32(&mut buf, 12, 1080);
    put_u32(&mut buf, 16, 1920 * 4);
    put_u32(&mut buf, 20, 4); // RGBA32
    put_u32(&mut buf, 24, 10);
    put_u32(&mut buf, 28, 20);
    put_u32(&mut buf, 32, 1930);
    put_u32(&mut buf, 36, 1100);
    put_u32(&mut buf, 40, 1); // H264
    put_u32(&mut buf, 64, 1); // fragment
    for i in HEADER_LEN..buf.len() {
        buf[i] = (i - HEADER_LEN) as u8;
    }
    let (packets, rest) = parse_packets(&buf);
    assert!(rest.is_empty());
    assert_eq!(packets.len(), 1);
    match &packets[0] {
        Packet::FrameBuffer(p) => {
            assert_eq!(p.width, 1920);
            assert_eq!(p.height, 1080);
            assert_eq!(p.format, 4);
            assert_eq!(p.compression, 1);
            assert_eq!(p.fragment_info, 1);
            assert_eq!(p.rect.x, 10);
            assert_eq!(p.rect.y, 20);
            assert_eq!(p.rect.width(), 1920);
            assert_eq!(p.rect.height(), 1080);
            assert_eq!(p.payload.len(), 64);
            assert_eq!(p.payload[3], 3);
        }
        other => panic!("unexpected packet {:?}", other),
    }
}

#[test]
fn parse_multiple_packets_in_one_message() {
    let mut buf = Vec::new();
    // visibility packet (no payload)
    let mut v = vec![0u8; HEADER_LEN];
    put_u32(&mut v, 0, 3);
    put_u32(&mut v, 4, 0);
    put_u32(&mut v, 8, 1);
    buf.extend_from_slice(&v);
    // cursor position with 4-byte payload
    let mut c = vec![0u8; HEADER_LEN + 4];
    put_u32(&mut c, 0, 4);
    put_u32(&mut c, 4, 4);
    put_u32(&mut c, 8, 100);
    put_u32(&mut c, 12, 200);
    put_u32(&mut c, 16, 1);
    buf.extend_from_slice(&c);
    let (packets, rest) = parse_packets(&buf);
    assert!(rest.is_empty());
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0], Packet::Visibility(true));
    match &packets[1] {
        Packet::CursorPosition { x, y, visible } => {
            assert_eq!((*x, *y, *visible), (100, 200, true));
        }
        other => panic!("unexpected {:?}", other),
    }
}

#[test]
fn parse_incomplete_packet_returns_remainder() {
    let mut buf = vec![0u8; HEADER_LEN + 10];
    put_u32(&mut buf, 0, 2);
    put_u32(&mut buf, 4, 100); // claims 100 payload bytes, only 10 present
    let (packets, rest) = parse_packets(&buf);
    assert!(packets.is_empty());
    assert_eq!(rest.len(), buf.len());
}
