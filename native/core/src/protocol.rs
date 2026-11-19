//! spacedesk binary protocol (protocol v4.8), reverse-engineered from the official
//! spacedesk HTML5 VIEWER v0.9.9z.63 (`src/spacedesk.min.js` in this repository).
//!
//! Transport: one WebSocket binary message per packet (the reference viewer assumes
//! this, but we tolerate several packets per message by walking `payload_len`).
//! Every packet is a 128-byte little-endian header followed by `payload_len` bytes:
//!   header[0..4]   = packet type (see [`HeaderType`])
//!   header[4..8]   = count of payload bytes following the header

pub const DEFAULT_PORT: u16 = 28252;
pub const HEADER_LEN: usize = 128;
pub const PAYLOAD_LEN: usize = 334;
pub const PACKET_LEN: usize = HEADER_LEN + PAYLOAD_LEN;
pub const KVM_HEADER_LEN: usize = 28;
pub const VERSION_MAJOR: u32 = 4;
pub const VERSION_MINOR: u32 = 8;

/// Wire transport to the spacedesk driver on port 28252.
/// The driver sniffs the first bytes: "GET ..." -> WebSocket mode,
/// anything else -> raw binary packets (native mode, used by the official
/// Windows/Android viewers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    Tcp,
    Ws,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum HeaderType {
    Identification = 0,
    Ping = 1,
    FrameBuffer = 2,
    Visibility = 3,
    CursorPosition = 4,
    CursorBitmap = 5,
    PowerSuspendHibernate = 6,
    FlowControlAck = 7,
    Disconnect = 8,
    Rotation = 9,
    Mouse = 10,
    Keyboard = 11,
    Touch = 12,
}

impl HeaderType {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Identification,
            1 => Self::Ping,
            2 => Self::FrameBuffer,
            3 => Self::Visibility,
            4 => Self::CursorPosition,
            5 => Self::CursorBitmap,
            6 => Self::PowerSuspendHibernate,
            7 => Self::FlowControlAck,
            8 => Self::Disconnect,
            9 => Self::Rotation,
            10 => Self::Mouse,
            11 => Self::Keyboard,
            12 => Self::Touch,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ClientType {
    WindowsRemoteMonitor = 0,
    WebBrowser = 1,
    UsbCableApple = 2,
    KvmWindows = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ColorType {
    Rgb8 = 0,
    Rgb16 = 1,
    Rgb24 = 2,
    RgbX32 = 3,
    Rgba32 = 4,
    Yuv444 = 5,
    Yuv422 = 6,
    Yuv420 = 7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Compression {
    Off = 0,
    H264 = 1,
    Jpeg = 3,
    MjpegD2 = 4,
}

impl Compression {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            0 => Self::Off,
            1 => Self::H264,
            3 => Self::Jpeg,
            4 => Self::MjpegD2,
            _ => return None,
        })
    }
}

pub mod mouse_events {
    pub const MOVE: u32 = 0x0001;
    pub const LEFT_DOWN: u32 = 0x0002;
    pub const LEFT_UP: u32 = 0x0004;
    pub const RIGHT_DOWN: u32 = 0x0008;
    pub const RIGHT_UP: u32 = 0x0010;
    pub const MIDDLE_DOWN: u32 = 0x0020;
    pub const MIDDLE_UP: u32 = 0x0040;
    pub const X_DOWN: u32 = 0x0080;
    pub const X_UP: u32 = 0x0100;
    pub const WHEEL: u32 = 0x0800;
    pub const HWHEEL: u32 = 0x1000;
    pub const ABSOLUTE: u32 = 0x8000;
    pub const VIRTUAL_DESK: u32 = 0x4000;
}

pub mod key_events {
    pub const EXTEND_KEY: u32 = 0x01;
    pub const KEY_UP: u32 = 0x02;
    pub const UNICODE: u32 = 0x04;
    pub const SCAN_CODE: u32 = 0x08;
}

pub mod touch_state {
    pub const UP: u32 = 0;
    pub const DOWN: u32 = 1;
    pub const NONE: u32 = 2;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub x2: i32,
    pub y2: i32,
}

impl Rect {
    pub fn width(&self) -> u32 {
        (self.x2 - self.x).max(0) as u32
    }
    pub fn height(&self) -> u32 {
        (self.y2 - self.y).max(0) as u32
    }
}

// ---------------------------------------------------------------------------
// little-endian helpers (the wire format is LE on all fields)
// ---------------------------------------------------------------------------

pub fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off] = (v & 0xFF) as u8;
    buf[off + 1] = ((v >> 8) & 0xFF) as u8;
}

pub fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off] = (v & 0xFF) as u8;
    buf[off + 1] = ((v >> 8) & 0xFF) as u8;
    buf[off + 2] = ((v >> 16) & 0xFF) as u8;
    buf[off + 3] = ((v >> 24) & 0xFF) as u8;
}

pub fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

pub fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

pub fn get_i32(buf: &[u8], off: usize) -> i32 {
    get_u32(buf, off) as i32
}

// ---------------------------------------------------------------------------
// incoming packet model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct FrameBufferPacket {
    pub payload_len: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub format: u32,
    pub rect: Rect,
    pub compression: u32,
    pub fragment_info: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorBitmapPacket {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub x_hot: i32,
    pub y_hot: i32,
    pub flags: u32,
    pub payload: Vec<u8>,
}

impl FrameBufferPacket {
    /// Tile dimensions: the rect when it is non-empty (native TCP mode sends the
    /// full screen size in width/height and the tile bounds in the rect), else
    /// the header resolution (WS viewer mode sends the tile size there).
    pub fn tile_dims(&self) -> (u32, u32) {
        if self.rect.x2 > self.rect.x && self.rect.y2 > self.rect.y {
            (self.rect.width(), self.rect.height())
        } else {
            (self.width.max(1), self.height.max(1))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    FrameBuffer(FrameBufferPacket),
    Visibility(bool),
    CursorPosition { x: i32, y: i32, visible: bool },
    CursorBitmap(CursorBitmapPacket),
    PowerSuspendHibernate(u32),
    Rotation(u32),
    Ping(Vec<u8>),
    Disconnect,
    Unknown { ty: u32, payload: Vec<u8> },
}

/// Parse as many complete packets as `buf` contains; returns the parsed packets
/// and the remaining (incomplete) bytes.
pub fn parse_packets(buf: &[u8]) -> (Vec<Packet>, &[u8]) {
    let mut out = Vec::new();
    let mut off = 0usize;
    while buf.len() - off >= HEADER_LEN {
        let header = &buf[off..off + HEADER_LEN];
        let ty = get_u32(header, 0);
        let payload_len = get_u32(header, 4) as usize;
        if buf.len() - off - HEADER_LEN < payload_len {
            break;
        }
        let payload = &buf[off + HEADER_LEN..off + HEADER_LEN + payload_len];
        let packet = match HeaderType::from_u32(ty) {
            Some(HeaderType::FrameBuffer) => Packet::FrameBuffer(FrameBufferPacket {
                payload_len: payload_len as u32,
                width: get_u32(header, 8),
                height: get_u32(header, 12),
                pitch: get_u32(header, 16),
                format: get_u32(header, 20),
                rect: Rect {
                    x: get_i32(header, 24),
                    y: get_i32(header, 28),
                    x2: get_i32(header, 32),
                    y2: get_i32(header, 36),
                },
                compression: get_u32(header, 40),
                fragment_info: get_u32(header, 64),
                payload: payload.to_vec(),
            }),
            Some(HeaderType::Visibility) => Packet::Visibility(get_u32(header, 8) == 1),
            Some(HeaderType::CursorPosition) => Packet::CursorPosition {
                x: get_i32(header, 8),
                y: get_i32(header, 12),
                visible: get_u32(header, 16) == 1,
            },
            Some(HeaderType::CursorBitmap) => Packet::CursorBitmap(CursorBitmapPacket {
                width: get_u32(header, 8),
                height: get_u32(header, 12),
                pitch: get_u32(header, 16),
                x_hot: get_i32(header, 20),
                y_hot: get_i32(header, 24),
                flags: get_u32(header, 28),
                payload: payload.to_vec(),
            }),
            Some(HeaderType::PowerSuspendHibernate) => {
                Packet::PowerSuspendHibernate(get_u32(header, 8))
            }
            Some(HeaderType::Rotation) => Packet::Rotation(get_u32(header, 8)),
            Some(HeaderType::Ping) => Packet::Ping(payload.to_vec()),
            Some(HeaderType::Disconnect) => Packet::Disconnect,
            _ => Packet::Unknown {
                ty,
                payload: payload.to_vec(),
            },
        };
        out.push(packet);
        off += HEADER_LEN + payload_len;
    }
    (out, &buf[off..])
}

// ---------------------------------------------------------------------------
// outgoing packet builders (client -> server)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Identification {
    pub transport: Transport,
    pub client_type: u32,
    pub compression: u32,
    pub quality: u32,
    pub subsampling: u32,
    /// frame rate mode for the WS viewer layout (0=30, 1=60); in TCP native
    /// mode the real fps value is sent instead.
    pub frame_rate: u16,
    pub fps: u16,
    pub version_major: u32,
    pub version_minor: u32,
    /// Resolution advertised at header offsets 52/88 (usually the local screen).
    pub resolution: (u32, u32),
    /// Optional custom resolution (header offsets 56/92, mode 2).
    pub resolution_custom: Option<(u32, u32)>,
    pub client_name: String,
}

impl Default for Identification {
    fn default() -> Self {
        Self {
            transport: Transport::Tcp,
            client_type: ClientType::WindowsRemoteMonitor as u32,
            compression: Compression::H264 as u32,
            quality: 70,
            subsampling: 0, // the HTML5 viewer leaves header offset 20 at 0
            frame_rate: 0,  // 0 = 30 fps, 1 = 60 fps (as in the HTML5 viewer)
            fps: 30,
            version_major: VERSION_MAJOR,
            version_minor: VERSION_MINOR,
            resolution: (1920, 1080),
            resolution_custom: None,
            client_name: "spacedesk native viewer (Linux)".to_string(),
        }
    }
}

/// Build the identification packet: 128-byte header + 334-byte payload.
///
/// Two layouts exist on the wire:
/// - `Transport::Ws`: exactly what the official HTML5 viewer sends (payload
///   magic UTF-16LE "1111122222", name at offset 78, fps mode at offset 44).
/// - `Transport::Tcp`: the "native" layout used by the official Windows/Android
///   viewers (payload starts with u32 1 + a UTF-16LE GUID, name at offset 78,
///   real fps at offset 44, flags 0x10003 at offset 36, 3 at offset 20, 2 at 28,
///   4 at offset 46) — confirmed working against current drivers by
///   third-party reverse-engineered clients.
pub fn identification_packet(ident: &Identification) -> Vec<u8> {
    let mut header = vec![0u8; HEADER_LEN];
    put_u32(&mut header, 0, HeaderType::Identification as u32);
    put_u32(&mut header, 4, PAYLOAD_LEN as u32);
    put_u32(&mut header, 8, ident.version_major);
    put_u32(&mut header, 12, ident.version_minor);
    put_u32(&mut header, 16, ident.client_type);
    put_u32(&mut header, 24, ident.compression);
    put_u32(&mut header, 32, ident.quality.clamp(1, 100));
    put_u32(&mut header, 52, ident.resolution.0);
    put_u32(&mut header, 88, ident.resolution.1);
    match ident.resolution_custom {
        Some((w, h)) => {
            put_u32(&mut header, 48, 2);
            put_u32(&mut header, 56, w);
            put_u32(&mut header, 92, h);
        }
        None => {
            put_u32(&mut header, 48, 1);
            put_u32(&mut header, 56, 0);
            put_u32(&mut header, 92, 0);
        }
    }

    let mut payload = vec![0u8; PAYLOAD_LEN];
    match ident.transport {
        Transport::Ws => {
            put_u32(&mut header, 20, ident.subsampling);
            put_u16(&mut header, 44, ident.frame_rate);
            put_u16(&mut header, 46, 0);
            // magic pattern of the reference viewer: UTF-16LE "11111" then "22222"
            for i in 0..5 {
                payload[i * 2] = 0x31;
                payload[10 + i * 2] = 0x32;
            }
        }
        Transport::Tcp => {
            put_u32(&mut header, 20, 3);
            put_u32(&mut header, 28, 2);
            put_u32(&mut header, 36, 0x0001_0003);
            put_u16(&mut header, 44, if ident.fps == 0 { 30 } else { ident.fps });
            put_u16(&mut header, 46, 4);
            // payload: u32 1, then a UTF-16LE GUID, then the client name at 78
            put_u32(&mut payload, 0, 1);
            let guid = format!(
                "{{{:08x}-{:04x}-{:04x}-{:04x}-{:012x}}}",
                pseudo_rand_u32(),
                pseudo_rand_u32() & 0xFFFF,
                (pseudo_rand_u32() & 0x0FFF) | 0x4000,
                (pseudo_rand_u32() & 0x3FFF) | 0x8000,
                pseudo_rand_u32()
            );
            let guid16: Vec<u16> = guid.encode_utf16().collect();
            for (i, ch) in guid16.iter().take(36).enumerate() {
                put_u16(&mut payload, 4 + i * 2, *ch);
            }
        }
    }
    // client name as UTF-16LE starting at offset 78
    let name: Vec<u16> = ident.client_name.encode_utf16().collect();
    for (i, ch) in name.iter().take((PAYLOAD_LEN - 78) / 2).enumerate() {
        put_u16(&mut payload, 78 + i * 2, *ch);
    }

    let mut packet = header;
    packet.extend_from_slice(&payload);
    packet
}

fn pseudo_rand_u32() -> u32 {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B9)
        ^ (std::process::id() as u64) << 32;
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    (seed >> 16) as u32
}

/// Flow-control ACK sent by the client after a complete frame (128-byte header only).
pub fn flow_control_ack() -> Vec<u8> {
    let mut header = vec![0u8; HEADER_LEN];
    put_u32(&mut header, 0, HeaderType::FlowControlAck as u32);
    put_u32(&mut header, 4, 0);
    header
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X,
}

fn kvm_header(ty: HeaderType) -> Vec<u8> {
    let mut header = vec![0u8; HEADER_LEN];
    put_u32(&mut header, 0, ty as u32);
    put_u32(&mut header, 4, 0);
    header
}

/// Relative mouse move: x/y are deltas in desktop pixels, flag MOVE.
pub fn mouse_move_packet(dx: i32, dy: i32) -> Vec<u8> {
    let mut p = kvm_header(HeaderType::Mouse);
    put_u32(&mut p, 8, dx as u32);
    put_u32(&mut p, 12, dy as u32);
    put_u32(&mut p, 20, mouse_events::MOVE);
    p
}

/// Absolute mouse move (0..65535 range on the server side), flag ABSOLUTE|MOVE.
pub fn mouse_absolute_packet(x: u32, y: u32) -> Vec<u8> {
    let mut p = kvm_header(HeaderType::Mouse);
    put_u32(&mut p, 8, x);
    put_u32(&mut p, 12, y);
    put_u32(&mut p, 20, mouse_events::ABSOLUTE | mouse_events::MOVE);
    p
}

pub fn mouse_button_packet(button: MouseButton, down: bool) -> Vec<u8> {
    let flag = match (button, down) {
        (MouseButton::Left, true) => mouse_events::LEFT_DOWN,
        (MouseButton::Left, false) => mouse_events::LEFT_UP,
        (MouseButton::Right, true) => mouse_events::RIGHT_DOWN,
        (MouseButton::Right, false) => mouse_events::RIGHT_UP,
        (MouseButton::Middle, true) => mouse_events::MIDDLE_DOWN,
        (MouseButton::Middle, false) => mouse_events::MIDDLE_UP,
        (MouseButton::X, true) => mouse_events::X_DOWN,
        (MouseButton::X, false) => mouse_events::X_UP,
    };
    let mut p = kvm_header(HeaderType::Mouse);
    put_u32(&mut p, 20, flag);
    p
}

/// Vertical wheel; `delta` follows the browser convention (+/-120 per notch).
pub fn mouse_wheel_packet(delta: i32) -> Vec<u8> {
    let mut p = kvm_header(HeaderType::Mouse);
    put_u32(&mut p, 16, delta as u32);
    put_u32(&mut p, 20, mouse_events::WHEEL);
    p
}

/// Keyboard event. `vk` is the Windows virtual-key code; sent as both vkey and
/// scancode with the SCAN_CODE flag, exactly like the HTML5 viewer.
pub fn keyboard_packet(vk: u32, down: bool, extended: bool) -> Vec<u8> {
    let mut flags = key_events::SCAN_CODE;
    if extended {
        flags |= key_events::EXTEND_KEY;
    }
    if !down {
        flags |= key_events::KEY_UP;
    }
    let mut p = kvm_header(HeaderType::Keyboard);
    put_u32(&mut p, 8, vk);
    put_u32(&mut p, 12, vk);
    put_u32(&mut p, 16, flags);
    put_u32(&mut p, 20, 0);
    put_u32(&mut p, 24, 0);
    p
}

/// Touch event (absolute coordinates in `res_x` x `res_y` space).
pub fn touch_packet(down: bool, x: u32, y: u32, res_x: u32, res_y: u32) -> Vec<u8> {
    let mut flags = 0x01; // absolute
    if down {
        flags |= 0x02;
    } else {
        flags |= 0x04;
    }
    let mut p = kvm_header(HeaderType::Touch);
    put_u32(&mut p, 8, x);
    put_u32(&mut p, 12, y);
    put_u32(&mut p, 16, res_x);
    put_u32(&mut p, 20, res_y);
    put_u32(&mut p, 24, flags);
    put_u32(&mut p, 28, 0);
    p
}

/// Disconnect packet (28-byte KVM header).
pub fn disconnect_packet() -> Vec<u8> {
    let mut header = vec![0u8; KVM_HEADER_LEN];
    put_u32(&mut header, 0, HeaderType::Disconnect as u32);
    header
}
