//! Session management: connect to a spacedesk server over WebSocket, perform the
//! identification handshake, reassemble fragmented frames, send flow-control ACKs
//! and forward KVM input commands.

use crate::decoder::is_complete_jpeg;
use crate::protocol::{
    self, CursorBitmapPacket, FrameBufferPacket, Identification, MouseButton,
};
use crate::ws::{WsClient, WsError, WsErrorKind, WsMessage};
use std::net::ToSocketAddrs;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub identification: Identification,
    pub connect_timeout: Duration,
    pub reconnect: bool,
    pub max_reconnects: u32,
    pub reconnect_delay: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: protocol::DEFAULT_PORT,
            identification: Identification::default(),
            connect_timeout: Duration::from_secs(10),
            reconnect: true,
            max_reconnects: 5,
            reconnect_delay: Duration::from_secs(2),
        }
    }
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

fn send_event(events: &Sender<ServerEvent>, ev: ServerEvent) -> bool {
    events.send(ev).is_ok()
}

/// Run the session (blocking). Intended to be spawned on its own thread:
/// `events` receives server events, `cmds` carries input commands to send.
pub fn run(
    cfg: &ClientConfig,
    events: Sender<ServerEvent>,
    cmds: Receiver<ClientCommand>,
) -> Result<(), WsError> {
    let mut attempts = 0u32;
    loop {
        match connect_once(cfg, &events, &cmds) {
            Ok(reason) => {
                send_event(&events, ServerEvent::Disconnected { reason });
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
) -> Result<String, WsError> {
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
    let mut ws = WsClient::connect(addr, &host_header, cfg.connect_timeout)?;
    ws.send_binary(&protocol::identification_packet(&cfg.identification))?;
    if !send_event(events, ServerEvent::Connected) {
        return Ok("event receiver dropped".to_string());
    }

    ws.set_read_timeout(Some(Duration::from_millis(15)))?;

    let mut leftover: Vec<u8> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut frag_count = 0u32;

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
                    let _ = ws.send_binary(&protocol::disconnect_packet());
                    let _ = ws.send_close(1000);
                    return Ok("disconnect requested".to_string());
                }
            };
            ws.send_binary(&packet)?;
        }

        match ws.read_message() {
            Ok(Some(WsMessage::Binary(data))) => {
                leftover.extend_from_slice(&data);
                let (packets, rest) = protocol::parse_packets(&leftover);
                leftover = rest.to_vec();
                for packet in packets {
                    match packet {
                        protocol::Packet::FrameBuffer(p) => {
                            let frag = p.fragment_info == 1;
                            if frag {
                                frag_count += 1;
                            }
                            if is_complete_jpeg(&p.payload) {
                                if !send_event(events, ServerEvent::Frame(p)) {
                                    return Ok("event receiver dropped".to_string());
                                }
                            } else {
                                pending.extend_from_slice(&p.payload);
                                if !frag || frag_count % 4 == 0 {
                                    let payload = std::mem::take(&mut pending);
                                    let mut p = p;
                                    p.payload = payload;
                                    if !send_event(events, ServerEvent::Frame(p)) {
                                        return Ok("event receiver dropped".to_string());
                                    }
                                }
                            }
                            if !frag || frag_count % 4 == 0 {
                                ws.send_binary(&protocol::flow_control_ack())?;
                            }
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
                            let _ = ws.send_close(1000);
                            return Ok("server sent disconnect".to_string());
                        }
                        protocol::Packet::Ping | protocol::Packet::Unknown { .. } => {}
                    }
                }
            }
            Ok(Some(WsMessage::Text(_))) => {}
            Ok(None) => return Ok("connection closed by server".to_string()),
            Err(e) => match e.kind {
                // read timeout: just loop back to drain input commands
                WsErrorKind::Timeout => continue,
                WsErrorKind::Closed => return Ok(e.detail),
                _ => return Err(e),
            },
        }
    }
}
