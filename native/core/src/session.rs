//! Session management: connect to a spacedesk driver (raw TCP or WebSocket on
//! port 28252), perform the identification handshake, reassemble fragmented
//! frames, echo keep-alive pings, send flow-control ACKs and forward KVM input.
//!
//! If the driver closes the connection immediately after the identification
//! packet (no frame received), [`run`] automatically retries with the next
//! identification in the ladder — native TCP first (what current drivers
//! expect from native viewers), then the HTML5-viewer WebSocket combination.

use crate::decoder::is_complete_jpeg;
use crate::protocol::{
    self, ClientType, Compression, CursorBitmapPacket, FrameBufferPacket, Identification,
    MouseButton, Transport,
};
use crate::transport::Conn;
use crate::ws::{WsError, WsErrorKind};
use std::net::ToSocketAddrs;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub identification: Identification,
    /// Tried in order when the server closes the connection right after the
    /// identification packet without sending any frame.
    pub fallback_identifications: Vec<Identification>,
    pub connect_timeout: Duration,
    pub reconnect: bool,
    pub max_reconnects: u32,
    pub reconnect_delay: Duration,
    pub verbose: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: protocol::DEFAULT_PORT,
            identification: Identification::default(),
            fallback_identifications: Vec::new(),
            connect_timeout: Duration::from_secs(10),
            reconnect: true,
            max_reconnects: 5,
            reconnect_delay: Duration::from_secs(2),
            verbose: false,
        }
    }
}

/// Standard identification ladder: native TCP + H264 first (best performance),
/// then native TCP + JPEG (what reverse-engineered native clients use), then
/// the HTML5-viewer WebSocket + JPEG combination, then WS + raw.
/// `primary_*` is tried first; the rest become fallbacks.
pub fn standard_ladder(
    primary_transport: Transport,
    primary_client_type: u32,
    primary_compression: u32,
) -> Vec<Identification> {
    let combos: [(Transport, u32, u32); 4] = [
        (Transport::Tcp, ClientType::WindowsRemoteMonitor as u32, Compression::H264 as u32),
        (Transport::Tcp, ClientType::WindowsRemoteMonitor as u32, Compression::Jpeg as u32),
        (Transport::Ws, ClientType::WebBrowser as u32, Compression::Jpeg as u32),
        (Transport::Ws, ClientType::WebBrowser as u32, Compression::Off as u32),
    ];
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let ordered = std::iter::once((primary_transport, primary_client_type, primary_compression))
        .chain(combos.iter().copied())
        .filter(|c| seen.insert((c.0, c.1, c.2)));
    for (tr, ct, comp) in ordered {
        let mut ident = Identification::default();
        ident.transport = tr;
        ident.client_type = ct;
        ident.compression = comp;
        out.push(ident);
    }
    out
}

#[derive(Debug)]
pub enum ServerEvent {
    Connected,
    /// A complete frame payload (fragments reassembled) ready to be decoded.
    Frame(FrameBufferPacket),
    Visibility(bool),
    CursorPosition { x: i32, y: i32, visible: bool },
    CursorBitmap(CursorBitmapPacket),
    PowerSuspendHibernate(u32),
    Rotation(u32),
    Disconnected { reason: String },
    Error(String),
}

#[derive(Debug, Clone)]
pub enum ClientCommand {
    MouseMove { dx: i32, dy: i32 },
    MouseAbsolute { x: u32, y: u32 },
    MouseButton { button: MouseButton, down: bool },
    MouseWheel { delta: i32 },
    Key { vk: u32, down: bool, extended: bool },
    Touch { down: bool, x: u32, y: u32, res_x: u32, res_y: u32 },
    Disconnect,
}

struct ConnectOutcome {
    reason: String,
    frames: u64,
    elapsed: Duration,
    close: Option<(u16, String)>,
}

fn send_event(events: &Sender<ServerEvent>, ev: ServerEvent) -> bool {
    events.send(ev).is_ok()
}

fn vlog(cfg: &ClientConfig, msg: &str) {
    if cfg.verbose {
        eprintln!("[session] {}", msg);
    }
}

/// Run the session (blocking). Intended to be spawned on its own thread:
/// `events` receives server events, `cmds` carries input commands to send.
pub fn run(
    cfg: &ClientConfig,
    events: Sender<ServerEvent>,
    cmds: Receiver<ClientCommand>,
) -> Result<(), WsError> {
    // identification ladder: primary first, then fallbacks
    let mut ladder = vec![cfg.identification.clone()];
    for f in &cfg.fallback_identifications {
        if !ladder
            .iter()
            .any(|i| i.transport == f.transport && i.client_type == f.client_type && i.compression == f.compression)
        {
            ladder.push(f.clone());
        }
    }
    let mut ladder_idx = 0usize;
    let mut attempts = 0u32;
    loop {
        let mut cfg_i = cfg.clone();
        cfg_i.identification = ladder[ladder_idx].clone();
        vlog(
            cfg,
            &format!(
                "connecting to {}:{} transport={:?} client_type={} compression={} (ladder {}/{})",
                cfg_i.host,
                cfg_i.port,
                cfg_i.identification.transport,
                cfg_i.identification.client_type,
                cfg_i.identification.compression,
                ladder_idx + 1,
                ladder.len()
            ),
        );
        match connect_once(&cfg_i, &events, &cmds) {
            Ok(outcome) => {
                let more = ladder_idx + 1 < ladder.len();
                let rejected = outcome.frames == 0 && outcome.elapsed < Duration::from_secs(3);
                let reason = match &outcome.close {
                    Some((code, r)) => format!("{} (close code {} reason {:?})", outcome.reason, code, r),
                    None => outcome.reason.clone(),
                };
                send_event(&events, ServerEvent::Disconnected { reason: reason.clone() });
                if rejected && more {
                    let prev = ladder[ladder_idx].clone();
                    ladder_idx += 1;
                    attempts = 0;
                    let next = &ladder[ladder_idx];
                    send_event(
                        &events,
                        ServerEvent::Error(format!(
                            "server rejected identification (transport={:?} client_type={} compression={}) with no frames; trying next: transport={:?} client_type={} compression={}",
                            prev.transport, prev.client_type, prev.compression,
                            next.transport, next.client_type, next.compression,
                        )),
                    );
                    continue;
                }
                if !cfg.reconnect || attempts >= cfg.max_reconnects {
                    return Ok(());
                }
            }
            Err(e) => {
                let retry = cfg.reconnect && attempts < cfg.max_reconnects;
                send_event(
                    &events,
                    ServerEvent::Error(format!("{} (reconnecting)", e)),
                );
                if !retry {
                    return Err(e);
                }
            }
        }
        attempts += 1;
        std::thread::sleep(cfg.reconnect_delay);
    }
}

fn connect_once(
    cfg: &ClientConfig,
    events: &Sender<ServerEvent>,
    cmds: &Receiver<ClientCommand>,
) -> Result<ConnectOutcome, WsError> {
    let start = Instant::now();
    let addr_str = format!("{}:{}", cfg.host, cfg.port);
    let addr = addr_str
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| WsError {
            kind: WsErrorKind::Io,
            detail: format!("cannot resolve {}", addr_str),
        })?;
    let host_header = if cfg.port == 80 {
        cfg.host.clone()
    } else {
        addr_str.clone()
    };
    let mut conn = Conn::connect(
        addr,
        &host_header,
        cfg.identification.transport,
        cfg.connect_timeout,
    )?;
    vlog(cfg, "connected, sending identification");
    conn.send_packet(&protocol::identification_packet(&cfg.identification))?;
    if !send_event(events, ServerEvent::Connected) {
        return Ok(ConnectOutcome {
            reason: "event receiver dropped".to_string(),
            frames: 0,
            elapsed: start.elapsed(),
            close: conn.last_close(),
        });
    }

    conn.set_read_timeout(Some(Duration::from_millis(15)))?;

    let mut leftover: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut frag_count = 0u32;
    let mut frames = 0u64;

    macro_rules! outcome {
        ($reason:expr) => {
            return Ok(ConnectOutcome {
                reason: $reason.to_string(),
                frames,
                elapsed: start.elapsed(),
                close: conn.last_close(),
            })
        };
    }

    loop {
        // drain input commands first (low latency input even while frames stream in)
        while let Ok(cmd) = cmds.try_recv() {
            let packet = match cmd {
                ClientCommand::MouseMove { dx, dy } => protocol::mouse_move_packet(dx, dy),
                ClientCommand::MouseAbsolute { x, y } => protocol::mouse_absolute_packet(x, y),
                ClientCommand::MouseButton { button, down } => {
                    protocol::mouse_button_packet(button, down)
                }
                ClientCommand::MouseWheel { delta } => protocol::mouse_wheel_packet(delta),
                ClientCommand::Key { vk, down, extended } => {
                    protocol::keyboard_packet(vk, down, extended)
                }
                ClientCommand::Touch { down, x, y, res_x, res_y } => {
                    protocol::touch_packet(down, x, y, res_x, res_y)
                }
                ClientCommand::Disconnect => {
                    let _ = conn.send_packet(&protocol::disconnect_packet());
                    let _ = conn.close(1000);
                    outcome!("disconnect requested");
                }
            };
            conn.send_packet(&packet)?;
        }

        match conn.read_message() {
            Ok(Some(data)) => {
                if data.is_empty() {
                    continue;
                }
                if cfg.verbose && data.len() >= 8 {
                    vlog(
                        cfg,
                        &format!(
                            "recv {} bytes, packet type {} payload_len {}",
                            data.len(),
                            protocol::get_u32(&data, 0),
                            protocol::get_u32(&data, 4)
                        ),
                    );
                }
                leftover.extend_from_slice(&data);
                let (packets, rest) = protocol::parse_packets(&leftover);
                leftover = rest.to_vec();
                for packet in packets {
                    match packet {
                        protocol::Packet::FrameBuffer(p) => {
                            frames += 1;
                            let frag = p.fragment_info == 1;
                            if frag {
                                frag_count += 1;
                            }
                            if is_complete_jpeg(&p.payload) {
                                if !send_event(events, ServerEvent::Frame(p)) {
                                    outcome!("event receiver dropped");
                                }
                            } else {
                                pending.extend_from_slice(&p.payload);
                                if !frag || frag_count % 4 == 0 {
                                    let payload = std::mem::take(&mut pending);
                                    let mut p = p;
                                    p.payload = payload;
                                    if !send_event(events, ServerEvent::Frame(p)) {
                                        outcome!("event receiver dropped");
                                    }
                                }
                            }
                            if !frag || frag_count % 4 == 0 {
                                conn.send_packet(&protocol::flow_control_ack())?;
                            }
                        }
                        protocol::Packet::Ping(payload) => {
                            // keep-alive: echo the ping back, otherwise the driver throttles
                            let mut echo = vec![0u8; protocol::HEADER_LEN];
                            protocol::put_u32(&mut echo, 0, protocol::HeaderType::Ping as u32);
                            protocol::put_u32(&mut echo, 4, payload.len() as u32);
                            echo.extend_from_slice(&payload);
                            let _ = conn.send_packet(&echo);
                        }
                        protocol::Packet::Visibility(v) => {
                            send_event(events, ServerEvent::Visibility(v));
                        }
                        protocol::Packet::CursorPosition { x, y, visible } => {
                            send_event(events, ServerEvent::CursorPosition { x, y, visible });
                        }
                        protocol::Packet::CursorBitmap(bmp) => {
                            send_event(events, ServerEvent::CursorBitmap(bmp));
                        }
                        protocol::Packet::PowerSuspendHibernate(v) => {
                            send_event(events, ServerEvent::PowerSuspendHibernate(v));
                        }
                        protocol::Packet::Rotation(v) => {
                            send_event(events, ServerEvent::Rotation(v));
                        }
                        protocol::Packet::Disconnect => {
                            let _ = conn.close(1000);
                            outcome!("server sent disconnect");
                        }
                        protocol::Packet::Unknown { .. } => {}
                    }
                }
            }
            Ok(None) => outcome!("connection closed by server"),
            Err(e) => match e.kind {
                // read timeout: just loop back to drain input commands
                WsErrorKind::Timeout => continue,
                WsErrorKind::Closed => outcome!(e.detail),
                _ => return Err(e),
            },
        }
    }
}
