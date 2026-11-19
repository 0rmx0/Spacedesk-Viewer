//! spacedesk-viewer: native high-performance spacedesk client for Linux
//! (developed and tested for Ubuntu 24.04).
//!
//! Pipeline: spacedesk protocol v4.8 (raw TCP or WebSocket) -> decode
//! (in-process JPEG tiles, FFmpeg/VA-API for H264, raw) -> OpenGL texture
//! compositing. Input (mouse/keyboard) is sent back as KVM packets.
//!
//! The app starts on a home menu (server IP, Connect button, settings panel);
//! once connected it switches to the video view. Esc returns to the menu.
//!
//! Usage: spacedesk-viewer [server-ip] [options]   (see --help); every option
//! only prefills the menu, nothing auto-connects.

mod font;
mod gl;
mod ui;
mod vkmap;

use gl::{cursor_payload_to_rgba, CursorImage, Renderer};
use glutin::config::ConfigTemplateBuilder;
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasRawWindowHandle;
use spacedesk_core::decoder::{Decoder, RawDecoder};
use spacedesk_core::ffmpeg::{codec_for_compression, ffmpeg_available, FfmpegDecoder, HwAccel};
use spacedesk_core::frame::DecodedFrame;
use spacedesk_core::protocol::{
    ClientType, Compression, CursorBitmapPacket, MouseButton, Transport,
};
use spacedesk_core::session::{run, standard_ladder, ClientCommand, ClientConfig, ServerEvent};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use ui::{Ui, COL_BG, COL_BORDER, COL_ERR, COL_OK, COL_PANEL, COL_TEXT, COL_TEXT_DIM};
use winit::event::{
    ElementState, Event, KeyEvent, MouseButton as WinitMouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowBuilder};

// ---------------------------------------------------------------------------
// settings (menu state)
// ---------------------------------------------------------------------------

const TRANSPORT_OPTS: [&str; 2] = ["tcp (native)", "ws (web viewer)"];
const CLIENT_TYPE_OPTS: [&str; 4] = [
    "0 windows remote",
    "1 web browser",
    "2 usb-c apple",
    "3 kvm windows",
];
const COMPRESSION_OPTS: [&str; 3] = ["h264", "jpeg", "off (raw)"];
const SUBSAMPLING_OPTS: [&str; 3] = ["0 = 4:4:4", "1 = 4:2:2", "2 = 4:2:0"];
const FPS_OPTS: [&str; 2] = ["30", "60"];
const HWACCEL_OPTS: [&str; 3] = ["auto", "vaapi", "none"];

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    host: String,
    port: String,
    transport: usize,
    client_type: usize,
    compression: usize,
    quality: String,
    subsampling: usize,
    fps: usize,
    /// index into RESOLUTION_OPTS; last entry = custom (res_custom)
    res_mode: usize,
    res_custom: String,
    win_w: String,
    win_h: String,
    fullscreen: bool,
    kvm: bool,
    reconnect: bool,
    verbose: bool,
    hwaccel: usize,
}

/// Resolution choices offered in the settings menu (last = custom WxH).
pub const RESOLUTION_OPTS: [&str; 8] = [
    "client (window size)",
    "1920x1080",
    "1600x900",
    "1366x768",
    "1280x720",
    "1024x768",
    "800x600",
    "custom...",
];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            host: String::new(),
            port: "28252".into(),
            transport: 0,
            client_type: 0,
            compression: 0,
            quality: "70".into(),
            subsampling: 2,
            fps: 1,
            res_mode: 0,
            res_custom: "1920x1080".into(),
            win_w: "1280".into(),
            win_h: "720".into(),
            fullscreen: false,
            kvm: true,
            reconnect: true,
            verbose: false,
            hwaccel: 0,
        }
    }
}

impl Settings {
    fn config_path() -> std::path::PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
            format!(
                "{}/.config",
                std::env::var("HOME").unwrap_or_else(|_| ".".into())
            )
        });
        std::path::PathBuf::from(base)
            .join("spacedesk-viewer")
            .join("settings.json")
    }

    fn load() -> Settings {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(txt) => serde_json::from_str(&txt).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    fn save(&self) {
        let path = Self::config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(txt) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, txt);
        }
    }

    /// Persist when the serialized settings changed since the last save.
    fn save_if_changed(&self, last_snap: &mut String) {
        if let Ok(snap) = serde_json::to_string(self) {
            if snap != *last_snap {
                *last_snap = snap;
                self.save();
            }
        }
    }

    fn resolution_str(&self) -> String {
        if self.res_mode == 0 {
            "client".to_string()
        } else if self.res_mode >= RESOLUTION_OPTS.len() - 1 {
            self.res_custom.trim().to_string()
        } else {
            RESOLUTION_OPTS[self.res_mode].to_string()
        }
    }

    fn hwaccel(&self) -> HwAccel {
        match self.hwaccel {
            1 => HwAccel::Vaapi("/dev/dri/renderD128".to_string()),
            2 => HwAccel::None,
            _ => HwAccel::Auto,
        }
    }

    fn window_size(&self) -> Result<(u32, u32), String> {
        let w: u32 = self.win_w.trim().parse().map_err(|_| "bad window width")?;
        let h: u32 = self.win_h.trim().parse().map_err(|_| "bad window height")?;
        if w < 320 || h < 240 || w > 7680 || h > 4320 {
            return Err("window size out of range (320x240..7680x4320)".into());
        }
        Ok((w, h))
    }

    fn client_config(&self) -> Result<ClientConfig, String> {
        let host = self.host.trim().to_string();
        if host.is_empty() {
            return Err("server IP is empty".into());
        }
        let port: u16 = self.port.trim().parse().map_err(|_| "bad port")?;
        let quality: u32 = self
            .quality
            .trim()
            .parse()
            .map_err(|_| "bad quality (1-100)")?;
        let (win_w, win_h) = self.window_size()?;
        let transport = if self.transport == 1 {
            Transport::Ws
        } else {
            Transport::Tcp
        };
        let compression = [
            Compression::H264 as u32,
            Compression::Jpeg as u32,
            Compression::Off as u32,
        ][self.compression.min(2)];
        let client_type = [
            ClientType::WindowsRemoteMonitor as u32,
            ClientType::WebBrowser as u32,
            ClientType::UsbCableApple as u32,
            ClientType::KvmWindows as u32,
        ][self.client_type.min(3)];

        let mut cfg = ClientConfig {
            host,
            port,
            reconnect: self.reconnect,
            verbose: self.verbose,
            ..Default::default()
        };
        let ladder = standard_ladder(transport, client_type, compression);
        cfg.identification = ladder[0].clone();
        cfg.identification.fps = if self.fps == 1 { 60 } else { 30 };
        cfg.identification.frame_rate = self.fps as u16;
        cfg.identification.client_type = client_type;
        cfg.identification.quality = quality;
        cfg.identification.subsampling = self.subsampling as u32;
        cfg.identification.resolution = (win_w, win_h);
        let res = self.resolution_str();
        if !res.is_empty() && res != "client" {
            let p: Vec<&str> = res.split('x').collect();
            if p.len() != 2 {
                return Err("resolution must be WxH or 'client'".into());
            }
            cfg.identification.resolution_custom = Some((
                p[0].trim().parse().map_err(|_| "bad resolution width")?,
                p[1].trim().parse().map_err(|_| "bad resolution height")?,
            ));
        }
        cfg.identification.client_name =
            format!("spacedesk-viewer {} (Linux)", env!("CARGO_PKG_VERSION"));
        cfg.fallback_identifications = ladder[1..].to_vec();
        Ok(cfg)
    }
}

fn help() -> &'static str {
    "spacedesk-viewer [server-ip] [options]\n\
     \nStarts the home menu (server IP + Connect + settings). Options only prefill the menu:\n\
     \x20 --port N            server port (default 28252)\n\
     \x20 --compression C     h264 | jpeg | off (default h264)\n\
     \x20 --quality N         1-100 (default 70)\n\
     \x20 --subsampling S     0=4:4:4 1=4:2:2 2=4:2:0 (default 2)\n\
     \x20 --fps F             30 | 60 (default 60)\n\
     \x20 --resolution WxH    custom resolution, or 'client' (default client)\n\
     \x20 --size WxH          window size (default 1280x720)\n\
     \x20 --fullscreen        start fullscreen once connected\n\
     \x20 --no-kvm            disable mouse/keyboard control\n\
     \x20 --hwaccel A         auto | vaapi | none (default auto)\n\
     \x20 --no-reconnect      do not reconnect on connection loss\n\
     \x20 --transport T       tcp | ws (default tcp)\n\
     \x20 --client-type N     0..3 (default 0)\n\
     \x20 -v, --verbose       verbose logging\n"
}

/// Lenient arg parsing: only prefills the menu settings, never fails hard.
fn settings_from_args() -> Settings {
    let mut s = Settings::load();
    let mut args = std::env::args().skip(1);
    let mut host: Option<String> = None;
    while let Some(a) = args.next() {
        let mut next = || args.next();
        match a.as_str() {
            "--help" | "-h" => {
                println!("{}", help());
                std::process::exit(0);
            }
            "--port" => {
                if let Some(v) = next() {
                    s.port = v;
                }
            }
            "--compression" => {
                if let Some(v) = next() {
                    s.compression = match v.as_str() {
                        "jpeg" => 1,
                        "off" | "raw" => 2,
                        _ => 0,
                    };
                }
            }
            "--quality" => {
                if let Some(v) = next() {
                    s.quality = v;
                }
            }
            "--subsampling" => {
                if let Some(v) = next() {
                    s.subsampling = v.parse().unwrap_or(2).min(2);
                }
            }
            "--fps" => {
                if let Some(v) = next() {
                    s.fps = if v == "30" { 0 } else { 1 };
                }
            }
            "--resolution" => {
                if let Some(v) = next() {
                    if v == "client" {
                        s.res_mode = 0;
                    } else {
                        s.res_mode = RESOLUTION_OPTS.len() - 1;
                        s.res_custom = v;
                    }
                }
            }
            "--size" => {
                if let Some(v) = next() {
                    let p: Vec<&str> = v.split('x').collect();
                    if p.len() == 2 {
                        s.win_w = p[0].to_string();
                        s.win_h = p[1].to_string();
                    }
                }
            }
            "--fullscreen" => s.fullscreen = true,
            "--no-kvm" => s.kvm = false,
            "--hwaccel" => {
                if let Some(v) = next() {
                    s.hwaccel = match v.as_str() {
                        "vaapi" => 1,
                        "none" => 2,
                        _ => 0,
                    };
                }
            }
            "--no-reconnect" => s.reconnect = false,
            "--verbose" | "-v" => s.verbose = true,
            "--transport" => {
                if let Some(v) = next() {
                    s.transport = if v == "ws" { 1 } else { 0 };
                }
            }
            "--client-type" => {
                if let Some(v) = next() {
                    s.client_type = v.parse().unwrap_or(0).min(3);
                }
            }
            h if host.is_none() && !h.starts_with('-') => host = Some(h.to_string()),
            _ => {}
        }
    }
    if let Some(h) = host.or_else(|| std::env::var("SPACEDESK_HOST").ok()) {
        s.host = h;
    }
    s
}

// ---------------------------------------------------------------------------
// app state
// ---------------------------------------------------------------------------

enum Mode {
    Menu,
    Video,
}

struct SessionHandle {
    stop: Arc<AtomicBool>,
    cmd: Sender<ClientCommand>,
    events: Receiver<ServerEvent>,
    join: JoinHandle<()>,
}

enum ActiveDecoder {
    Raw(RawDecoder),
    Jpeg(spacedesk_core::jpeg::JpegDecoder),
    Ffmpeg(FfmpegDecoder),
}

/// Log to stderr without ever panicking: when stdout/stderr is a pipe closed
/// early (e.g. `... -v 2>&1 | head -20`), eprintln! would panic on EPIPE and
/// kill the thread (or the whole process on the main thread).
fn log_err(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "{}", msg);
}

struct AppState {
    mode: Mode,
    settings: Settings,
    show_settings: bool,
    ui: Ui,
    status: String,
    status_ok: bool,
    session: Option<SessionHandle>,
    // video state: decoding runs on a worker thread so the UI thread never
    // stalls on CPU JPEG decode (source of freezes/lag)
    pkt_tx: Option<SyncSender<spacedesk_core::protocol::FrameBufferPacket>>,
    frame_rx: Option<Receiver<Vec<DecodedFrame>>>,
    decode_stop: Option<Arc<AtomicBool>>,
    decode_join: Option<JoinHandle<()>>,
    pending_tiles: Vec<DecodedFrame>,
    last_snap: String,
    screen: (u32, u32),
    cursor: Option<CursorImage>,
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    hwaccel: HwAccel,
    frame_rate: u16,
    kvm: bool,
    last_mouse: Option<(f64, f64)>,
    grabbed: bool,
    // per-frame input queues
    mouse_pos: (f32, f32),
    mouse_down: bool,
    chars: Vec<char>,
    backspace: bool,
    enter: bool,
    quit_requested: bool,
}

impl AppState {
    /// Surface decode errors (first few) instead of failing silently — a black
    /// window with no message is indistinguishable from "nothing works".
    fn start_session(&mut self, window: &Window) {
        self.stop_session();
        let cfg = match self.settings.client_config() {
            Ok(c) => c,
            Err(e) => {
                self.status = format!("invalid settings: {}", e);
                self.status_ok = false;
                return;
            }
        };
        let (win_w, win_h) = self.settings.window_size().unwrap_or((1280, 720));
        let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(win_w, win_h));
        if self.settings.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            window.set_fullscreen(None);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let mut cfg = cfg;
        cfg.stop = Some(stop.clone());
        let (ev_tx, ev_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
        let join = std::thread::spawn(move || {
            let _ = run(&cfg, ev_tx, cmd_rx);
        });
        self.session = Some(SessionHandle {
            stop,
            cmd: cmd_tx,
            events: ev_rx,
            join,
        });
        self.mode = Mode::Video;
        self.pending_tiles.clear();
        self.screen = (1, 1);
        self.hwaccel = self.settings.hwaccel();
        self.frame_rate = self.settings.fps as u16;
        self.kvm = self.settings.kvm;
        // decode worker thread
        // bounded channel: if the decoder falls behind, stale packets are
        // dropped instead of accumulating into seconds of latency
        let (pkt_tx, pkt_rx) =
            mpsc::sync_channel::<spacedesk_core::protocol::FrameBufferPacket>(64);
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<DecodedFrame>>();
        let dstop = Arc::new(AtomicBool::new(false));
        let hwaccel = self.hwaccel.clone();
        let frame_rate = self.frame_rate;
        let dstop2 = dstop.clone();
        let djoin = std::thread::spawn(move || {
            let mut decoder: Option<ActiveDecoder> = None;
            while !dstop2.load(Ordering::SeqCst) {
                let p0 = match pkt_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(p) => p,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let want = codec_for_compression(p0.compression);
                let p = if matches!(want, Some(spacedesk_core::ffmpeg::CodecKind::Mjpeg)) {
                    // JPEG tiles are self-contained: if we're behind, skip the
                    // queued older tiles and decode only the freshest ones.
                    // (H264/raw are byte streams — every packet must be fed.)
                    let mut p = p0;
                    // drop up to 8 queued older tiles, keep the freshest
                    for _ in 0..8 {
                        match pkt_rx.try_recv() {
                            Ok(newer) => p = newer,
                            Err(_) => break,
                        }
                    }
                    p
                } else {
                    p0
                };
                let need_new = match (&decoder, want) {
                    (None, _) => true,
                    (Some(ActiveDecoder::Raw(_)), None) => false,
                    (Some(ActiveDecoder::Raw(_)), Some(_)) => true,
                    (
                        Some(ActiveDecoder::Jpeg(_)),
                        Some(spacedesk_core::ffmpeg::CodecKind::Mjpeg),
                    ) => false,
                    (Some(ActiveDecoder::Jpeg(_)), _) => true,
                    (Some(ActiveDecoder::Ffmpeg(_)), None) => true,
                    (Some(ActiveDecoder::Ffmpeg(f)), Some(c)) => f.codec_kind() != c,
                };
                if need_new {
                    decoder = match want {
                        None => Some(ActiveDecoder::Raw(RawDecoder)),
                        Some(spacedesk_core::ffmpeg::CodecKind::Mjpeg) => {
                            Some(ActiveDecoder::Jpeg(spacedesk_core::jpeg::JpegDecoder))
                        }
                        Some(codec) => {
                            let (tw, th) = (p.width.max(1), p.height.max(1));
                            match FfmpegDecoder::new(
                                codec,
                                hwaccel.clone(),
                                tw,
                                th,
                                if frame_rate == 1 { 60 } else { 30 },
                            ) {
                                Ok(d) => Some(ActiveDecoder::Ffmpeg(d)),
                                Err(e) => {
                                    log_err(&format!("decoder init failed: {}", e));
                                    None
                                }
                            }
                        }
                    };
                }
                let frames = match &mut decoder {
                    Some(ActiveDecoder::Raw(d)) => d.submit(&p).unwrap_or_default(),
                    Some(ActiveDecoder::Jpeg(d)) => match d.submit(&p) {
                        Ok(f) => f,
                        Err(e) => {
                            log_err(&format!("jpeg decode: {}", e));
                            Vec::new()
                        }
                    },
                    Some(ActiveDecoder::Ffmpeg(d)) => d.submit(&p).unwrap_or_default(),
                    None => Vec::new(),
                };
                if !frames.is_empty() && frame_tx.send(frames).is_err() {
                    break;
                }
            }
        });
        self.pkt_tx = Some(pkt_tx);
        self.frame_rx = Some(frame_rx);
        self.decode_stop = Some(dstop);
        self.decode_join = Some(djoin);
        self.status = format!(
            "connecting to {}:{} ...",
            self.settings.host.trim(),
            self.settings.port.trim()
        );
        self.status_ok = true;
        window.set_title("spacedesk viewer - connecting...");
        if self.kvm {
            let _ = window.set_cursor_grab(CursorGrabMode::Confined);
            window.set_cursor_visible(false);
            self.grabbed = true;
        }
    }

    fn stop_session(&mut self) {
        if let Some(s) = self.session.take() {
            s.stop.store(true, Ordering::SeqCst);
            let _ = s.cmd.send(ClientCommand::Disconnect);
            let _ = s.join.join();
        }
        if let Some(stop) = self.decode_stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        drop(self.pkt_tx.take());
        self.frame_rx.take();
        if let Some(j) = self.decode_join.take() {
            let _ = j.join();
        }
    }

    fn back_to_menu(&mut self, window: &Window, reason: &str) {
        self.stop_session();
        self.mode = Mode::Menu;
        self.status = reason.to_string();
        self.status_ok = false;
        self.pending_tiles.clear();
        self.screen = (1, 1);
        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor_visible(true);
        self.grabbed = false;
        window.set_fullscreen(None);
        window.set_title("spacedesk viewer");
    }

    fn drain_events(&mut self, window: &Window) {
        let events: Vec<ServerEvent> = match &self.session {
            Some(s) => {
                let mut v = Vec::new();
                while let Ok(ev) = s.events.try_recv() {
                    v.push(ev);
                }
                v
            }
            None => return,
        };
        for ev in events {
            {
                match ev {
                    ServerEvent::Connected => {
                        self.status = "connected".to_string();
                        self.status_ok = true;
                        window.set_title("spacedesk viewer - connected");
                    }
                    ServerEvent::Error(e) => {
                        self.status = format!("error: {}", e);
                        self.status_ok = false;
                        log_err(&format!("error: {}", e));
                    }
                    ServerEvent::Disconnected { reason } => {
                        log_err(&format!("disconnected: {}", reason));
                        if matches!(self.mode, Mode::Video) {
                            let r = format!("disconnected: {}", reason);
                            // borrow workaround: stop session after the loop
                            self.back_to_menu(window, &r);
                        } else {
                            self.status = format!("disconnected: {}", reason);
                            self.status_ok = false;
                        }
                    }
                    ServerEvent::Visibility(_)
                    | ServerEvent::PowerSuspendHibernate(_)
                    | ServerEvent::Rotation(_) => {}
                    ServerEvent::CursorPosition { x, y, visible } => {
                        self.cursor_pos = (x, y);
                        self.cursor_visible = visible;
                    }
                    ServerEvent::CursorBitmap(bmp) => {
                        self.cursor = Some(cursor_from_packet(&bmp));
                    }
                    ServerEvent::Frame(p) => {
                        if matches!(self.mode, Mode::Video) {
                            self.handle_frame(p, window);
                        }
                    }
                }
            }
        }
    }

    fn handle_frame(&mut self, p: spacedesk_core::protocol::FrameBufferPacket, _window: &Window) {
        // native TCP mode: header = full screen size, rect = tile bounds.
        let sw = p.width.max(p.rect.x2.max(0) as u32).max(1);
        let sh = p.height.max(p.rect.y2.max(0) as u32).max(1);
        if sw * sh > self.screen.0 * self.screen.1 {
            self.screen = (sw, sh);
        }
        if matches!(self.mode, Mode::Video) {
            if let Some(tx) = &self.pkt_tx {
                // bounded channel: if the worker is saturated, drop this
                // (now stale) packet rather than growing the backlog
                let _ = tx.try_send(p);
            }
        }
    }

    /// Pull decoded tiles from the worker; cap the queue so latency stays low.
    fn drain_decoded(&mut self, window: &Window) {
        let Some(rx) = &self.frame_rx else { return };
        let mut got = false;
        while let Ok(frames) = rx.try_recv() {
            const MAX_PENDING: usize = 24;
            if self.pending_tiles.len() + frames.len() > MAX_PENDING {
                let drop_n = (self.pending_tiles.len() + frames.len() - MAX_PENDING)
                    .min(self.pending_tiles.len());
                self.pending_tiles.drain(..drop_n);
            }
            self.pending_tiles.extend(frames);
            got = true;
        }
        if got {
            window.request_redraw();
        }
    }
}

fn cursor_from_packet(bmp: &CursorBitmapPacket) -> CursorImage {
    CursorImage {
        width: bmp.width,
        height: bmp.height,
        x_hot: bmp.x_hot,
        y_hot: bmp.y_hot,
        rgba: cursor_payload_to_rgba(bmp.width, bmp.height, bmp.pitch, bmp.flags, &bmp.payload),
    }
}

// ---------------------------------------------------------------------------
// menu drawing
// ---------------------------------------------------------------------------

// widget ids
const ID_HOST: u32 = 1;
const ID_PORT: u32 = 2;
const ID_CONNECT: u32 = 3;
const ID_SETTINGS: u32 = 4;
const ID_QUIT: u32 = 5;
const ID_TRANSPORT: u32 = 10;
const ID_CLIENT_TYPE: u32 = 11;
const ID_COMPRESSION: u32 = 12;
const ID_QUALITY: u32 = 13;
const ID_SUBSAMPLING: u32 = 14;
const ID_FPS: u32 = 15;
const ID_RESOLUTION: u32 = 16;
const ID_RES_CUSTOM: u32 = 24;
const ID_WIN_W: u32 = 17;
const ID_WIN_H: u32 = 18;
const ID_HWACCEL: u32 = 19;
const ID_FULLSCREEN: u32 = 20;
const ID_KVM: u32 = 21;
const ID_RECONNECT: u32 = 22;
const ID_VERBOSE: u32 = 23;

fn draw_menu(state: &mut AppState, renderer: &Renderer, viewport: (u32, u32), window: &Window) {
    let ui = &mut state.ui;
    let s = &mut state.settings;
    unsafe {
        renderer.begin_ui(viewport);
        renderer.draw_rect(0.0, 0.0, viewport.0 as f32, viewport.1 as f32, COL_BG);

        // title
        renderer.draw_text(40.0, 78.0, 2.0, "spacedesk viewer", COL_TEXT);
        renderer.draw_text(
            42.0,
            104.0,
            1.0,
            "native Linux client - protocol v4.8",
            COL_TEXT_DIM,
        );

        // connection row
        let row_y = 140.0;
        renderer.draw_text(40.0, row_y + 19.0, 1.0, "Server IP:", COL_TEXT_DIM);
        let enter_host = ui.text_field(
            renderer,
            ID_HOST,
            130.0,
            row_y,
            240.0,
            &mut s.host,
            false,
            "e.g. 192.168.1.10",
        );
        renderer.draw_text(390.0, row_y + 19.0, 1.0, "Port:", COL_TEXT_DIM);
        let enter_port = ui.text_field(
            renderer,
            ID_PORT,
            435.0,
            row_y,
            80.0,
            &mut s.port,
            true,
            "28252",
        );
        let connect = ui.button(
            renderer,
            ID_CONNECT,
            535.0,
            row_y - 4.0,
            130.0,
            34.0,
            "Connect",
            true,
        );
        let settings_btn = ui.button(
            renderer,
            ID_SETTINGS,
            680.0,
            row_y - 4.0,
            120.0,
            34.0,
            if state.show_settings {
                "Hide settings"
            } else {
                "Settings"
            },
            false,
        );
        let quit = ui.button(
            renderer,
            ID_QUIT,
            815.0,
            row_y - 4.0,
            90.0,
            34.0,
            "Quit",
            false,
        );
        if settings_btn {
            state.show_settings = !state.show_settings;
        }

        // status line
        let (scol, status) = if state.status.is_empty() {
            (
                COL_TEXT_DIM,
                "enter the server address, then Connect".to_string(),
            )
        } else {
            (
                if state.status_ok { COL_OK } else { COL_ERR },
                state.status.clone(),
            )
        };
        renderer.draw_text(40.0, 205.0, 1.0, &status, scol);

        // settings panel
        if !state.show_settings {
            ui.close_popup();
        }
        if state.show_settings {
            let px = 40.0;
            let py = 230.0;
            let pw = (viewport.0 as f32 - 80.0).max(600.0);
            let ph = 320.0;
            renderer.draw_rect(px, py, pw, ph, COL_PANEL);
            renderer.draw_rect(px, py, pw, 1.0, COL_BORDER);
            renderer.draw_rect(px, py + ph - 1.0, pw, 1.0, COL_BORDER);
            renderer.draw_rect(px, py, 1.0, ph, COL_BORDER);
            renderer.draw_rect(px + pw - 1.0, py, 1.0, ph, COL_BORDER);
            renderer.draw_text(px + 16.0, py + 26.0, 1.0, "Settings", COL_TEXT);

            let col1 = px + 20.0;
            let col2 = px + pw / 2.0 + 10.0;
            let label_w = 150.0;
            let ctrl_x1 = col1 + label_w;
            let ctrl_x2 = col2 + label_w;
            let r0 = py + 44.0;
            let step = 40.0;

            // column 1
            let y = r0;
            renderer.draw_text(col1, y + 19.0, 1.0, "Transport", COL_TEXT_DIM);
            ui.dropdown(
                renderer,
                ID_TRANSPORT,
                ctrl_x1,
                y,
                200.0,
                &TRANSPORT_OPTS,
                &mut s.transport,
            );
            let y = r0 + step;
            renderer.draw_text(col1, y + 19.0, 1.0, "Client type", COL_TEXT_DIM);
            ui.dropdown(
                renderer,
                ID_CLIENT_TYPE,
                ctrl_x1,
                y,
                200.0,
                &CLIENT_TYPE_OPTS,
                &mut s.client_type,
            );
            let y = r0 + step * 2.0;
            renderer.draw_text(col1, y + 19.0, 1.0, "Compression", COL_TEXT_DIM);
            ui.dropdown(
                renderer,
                ID_COMPRESSION,
                ctrl_x1,
                y,
                200.0,
                &COMPRESSION_OPTS,
                &mut s.compression,
            );
            let y = r0 + step * 3.0;
            renderer.draw_text(col1, y + 19.0, 1.0, "Quality (1-100)", COL_TEXT_DIM);
            ui.text_field(
                renderer,
                ID_QUALITY,
                ctrl_x1,
                y,
                100.0,
                &mut s.quality,
                true,
                "70",
            );
            let y = r0 + step * 4.0;
            renderer.draw_text(col1, y + 19.0, 1.0, "Subsampling", COL_TEXT_DIM);
            ui.dropdown(
                renderer,
                ID_SUBSAMPLING,
                ctrl_x1,
                y,
                200.0,
                &SUBSAMPLING_OPTS,
                &mut s.subsampling,
            );
            let y = r0 + step * 5.0;
            renderer.draw_text(col1, y + 19.0, 1.0, "FPS", COL_TEXT_DIM);
            ui.dropdown(renderer, ID_FPS, ctrl_x1, y, 100.0, &FPS_OPTS, &mut s.fps);

            // column 2
            let y = r0;
            renderer.draw_text(col2, y + 19.0, 1.0, "Resolution", COL_TEXT_DIM);
            ui.dropdown(
                renderer,
                ID_RESOLUTION,
                ctrl_x2,
                y,
                200.0,
                &RESOLUTION_OPTS,
                &mut s.res_mode,
            );
            if s.res_mode == RESOLUTION_OPTS.len() - 1 {
                ui.text_field(
                    renderer,
                    ID_RES_CUSTOM,
                    ctrl_x2 + 210.0,
                    y,
                    110.0,
                    &mut s.res_custom,
                    false,
                    "WxH",
                );
            }
            let y = r0 + step;
            renderer.draw_text(col2, y + 19.0, 1.0, "Window size", COL_TEXT_DIM);
            ui.text_field(
                renderer,
                ID_WIN_W,
                ctrl_x2,
                y,
                80.0,
                &mut s.win_w,
                true,
                "1280",
            );
            renderer.draw_text(ctrl_x2 + 86.0, y + 19.0, 1.0, "x", COL_TEXT_DIM);
            ui.text_field(
                renderer,
                ID_WIN_H,
                ctrl_x2 + 100.0,
                y,
                80.0,
                &mut s.win_h,
                true,
                "720",
            );
            let y = r0 + step * 2.0;
            renderer.draw_text(col2, y + 19.0, 1.0, "HW accel", COL_TEXT_DIM);
            ui.dropdown(
                renderer,
                ID_HWACCEL,
                ctrl_x2,
                y,
                200.0,
                &HWACCEL_OPTS,
                &mut s.hwaccel,
            );
            let y = r0 + step * 3.0;
            ui.checkbox(
                renderer,
                ID_FULLSCREEN,
                col2,
                y,
                "start fullscreen",
                &mut s.fullscreen,
            );
            ui.checkbox(
                renderer,
                ID_KVM,
                col2 + 220.0,
                y,
                "mouse/keyboard control (KVM)",
                &mut s.kvm,
            );
            let y = r0 + step * 4.0;
            ui.checkbox(
                renderer,
                ID_RECONNECT,
                col2,
                y,
                "auto-reconnect",
                &mut s.reconnect,
            );
            ui.checkbox(
                renderer,
                ID_VERBOSE,
                col2 + 220.0,
                y,
                "verbose logs",
                &mut s.verbose,
            );
            let y = r0 + step * 5.0;
            renderer.draw_text(
                col2,
                y + 19.0,
                1.0,
                "Esc: back to menu - F11: fullscreen (in video)",
                COL_TEXT_DIM,
            );
        }

        ui.draw_popups(renderer);

        renderer.end_ui();

        state.settings.save_if_changed(&mut state.last_snap);

        if connect || enter_host || enter_port || (ui.enter && ui.focus != 0) {
            state.start_session(window);
        }
        if quit {
            state.quit_requested = true;
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let settings = settings_from_args();
    // settings.compression is a dropdown index: 0 = h264, 1 = jpeg, 2 = off
    if settings.compression == 0 && !ffmpeg_available() {
        log_err("warning: ffmpeg not found; H264 decoding needs it (`sudo apt install ffmpeg`) - JPEG/raw still work");
    }

    let (win_w, win_h) = settings.window_size().unwrap_or((1280, 720));
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let window_builder = WindowBuilder::new()
        .with_title("spacedesk viewer")
        .with_inner_size(winit::dpi::LogicalSize::new(win_w, win_h));
    let template = ConfigTemplateBuilder::new();
    let display_builder = DisplayBuilder::new().with_window_builder(Some(window_builder));
    let (window, gl_config) = display_builder
        .build(&event_loop, template, |configs| {
            configs
                .reduce(|accum, config| {
                    if config.num_samples() > accum.num_samples() {
                        config
                    } else {
                        accum
                    }
                })
                .unwrap()
        })
        .expect("failed to create window/GL config");
    let window = window.expect("no window");

    let attrs = window.build_surface_attributes(Default::default());
    let gl_surface = unsafe {
        gl_config
            .display()
            .create_window_surface(&gl_config, &attrs)
    }
    .unwrap();
    let gl_context = unsafe {
        gl_config
            .display()
            .create_context(
                &gl_config,
                &glutin::context::ContextAttributesBuilder::new()
                    .with_context_api(glutin::context::ContextApi::OpenGl(Some(
                        glutin::context::Version::new(3, 3),
                    )))
                    .build(Some(window.raw_window_handle())),
            )
            .unwrap()
    };
    let gl_context = gl_context.make_current(&gl_surface).unwrap();
    gl_surface
        .set_swap_interval(
            &gl_context,
            glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()),
        )
        .unwrap();
    let gl = unsafe {
        glow::Context::from_loader_function(|s| {
            gl_config
                .display()
                .get_proc_address(&std::ffi::CString::new(s).unwrap())
        })
    };
    let mut renderer = unsafe { Renderer::new(gl) };
    if let Some(atlas) = font::build_atlas() {
        unsafe { renderer.set_font(&atlas) };
    } else {
        log_err("warning: no system TTF font found, menu text will not render");
    }

    let last_snap = serde_json::to_string(&settings).unwrap_or_default();
    let mut state = AppState {
        mode: Mode::Menu,
        show_settings: false,
        settings,
        ui: Ui::new(),
        status: String::new(),
        status_ok: true,
        session: None,
        pkt_tx: None,
        frame_rx: None,
        decode_stop: None,
        decode_join: None,
        pending_tiles: Vec::new(),
        screen: (1, 1),
        cursor: None,
        cursor_pos: (0, 0),
        cursor_visible: false,
        hwaccel: HwAccel::Auto,
        frame_rate: 1,
        kvm: true,
        last_mouse: None,
        grabbed: false,
        mouse_pos: (0.0, 0.0),
        mouse_down: false,
        chars: Vec::new(),
        backspace: false,
        enter: false,
        quit_requested: false,
        last_snap,
    };

    let result = event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Poll);
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    state.settings.save();
                    state.stop_session();
                    elwt.exit();
                }
                WindowEvent::RedrawRequested => {
                    let size = window.inner_size();
                    let viewport = (size.width.max(1), size.height.max(1));
                    unsafe {
                        match state.mode {
                            Mode::Menu => {
                                let chars = std::mem::take(&mut state.chars);
                                let backspace = std::mem::take(&mut state.backspace);
                                let enter = std::mem::take(&mut state.enter);
                                state.ui.begin(
                                    state.mouse_pos,
                                    state.mouse_down,
                                    chars,
                                    backspace,
                                    enter,
                                );
                                draw_menu(&mut state, &renderer, viewport, &window);
                                if state.quit_requested {
                                    state.settings.save();
                                    state.stop_session();
                                    elwt.exit();
                                }
                            }
                            Mode::Video => {
                                if !state.pending_tiles.is_empty() {
                                    for tile in state.pending_tiles.drain(..) {
                                        renderer.composite_tile(&tile, state.screen);
                                    }
                                }
                                if state.screen != (1, 1) {
                                    let cursor = if state.cursor_visible {
                                        state
                                            .cursor
                                            .as_ref()
                                            .map(|c| (c, state.cursor_pos.0, state.cursor_pos.1))
                                    } else {
                                        None
                                    };
                                    renderer.present(viewport, cursor);
                                } else {
                                    renderer.clear_screen();
                                }
                            }
                        }
                        let _ = gl_surface.swap_buffers(&gl_context);
                    }
                }
                WindowEvent::Resized(size) => {
                    if let (Some(w), Some(h)) =
                        (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
                    {
                        gl_surface.resize(&gl_context, w, h);
                    }
                    window.request_redraw();
                }
                WindowEvent::CursorMoved { position, .. } => {
                    state.mouse_pos = (position.x as f32, position.y as f32);
                    if state.kvm && matches!(state.mode, Mode::Video) {
                        let inner = window.inner_size();
                        if let Some(last) = state.last_mouse {
                            let scale_x = state.screen.0 as f64 / inner.width.max(1) as f64;
                            let scale_y = state.screen.1 as f64 / inner.height.max(1) as f64;
                            let dx = ((position.x - last.0) * scale_x) as i32;
                            let dy = ((position.y - last.1) * scale_y) as i32;
                            if dx != 0 || dy != 0 {
                                if let Some(s) = &state.session {
                                    let _ = s.cmd.send(ClientCommand::MouseMove { dx, dy });
                                }
                            }
                        }
                        state.last_mouse = Some((position.x, position.y));
                    }
                    window.request_redraw();
                }
                WindowEvent::MouseInput {
                    state: st, button, ..
                } => {
                    let down = st == ElementState::Pressed;
                    state.mouse_down = down;
                    if state.kvm && matches!(state.mode, Mode::Video) {
                        let btn = match button {
                            WinitMouseButton::Left => MouseButton::Left,
                            WinitMouseButton::Right => MouseButton::Right,
                            WinitMouseButton::Middle => MouseButton::Middle,
                            _ => MouseButton::X,
                        };
                        if let Some(s) = &state.session {
                            let _ = s.cmd.send(ClientCommand::MouseButton { button: btn, down });
                        }
                    }
                    window.request_redraw();
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    if state.kvm && matches!(state.mode, Mode::Video) {
                        let d = match delta {
                            MouseScrollDelta::LineDelta(_, y) => (y * 120.0) as i32,
                            MouseScrollDelta::PixelDelta(p) => p.y as i32,
                        };
                        if d != 0 {
                            if let Some(s) = &state.session {
                                let _ = s.cmd.send(ClientCommand::MouseWheel { delta: d });
                            }
                        }
                    }
                }
                WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                    if matches!(state.mode, Mode::Menu) {
                        state.chars.extend(text.chars().filter(|c| !c.is_control()));
                        window.request_redraw();
                    }
                }
                WindowEvent::KeyboardInput {
                    event:
                        KeyEvent {
                            state: st,
                            physical_key: PhysicalKey::Code(code),
                            text,
                            ..
                        },
                    ..
                } => {
                    match state.mode {
                        Mode::Menu => {
                            if st == ElementState::Pressed {
                                // character input: winit fills KeyEvent.text via
                                // xkbcommon on X11 and Wayland (no IME needed)
                                if let Some(t) = &text {
                                    state.chars.extend(t.chars().filter(|c| !c.is_control()));
                                }
                                match code {
                                    KeyCode::Backspace => state.backspace = true,
                                    KeyCode::Enter | KeyCode::NumpadEnter => state.enter = true,
                                    KeyCode::Escape => state.ui.open_popup = 0,
                                    _ => {}
                                }
                            }
                            window.request_redraw();
                        }
                        Mode::Video => {
                            if code == KeyCode::F11 && st == ElementState::Pressed {
                                let fs = window.fullscreen();
                                if fs.is_some() {
                                    window.set_fullscreen(None);
                                } else {
                                    window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                                }
                                return;
                            }
                            if code == KeyCode::Escape && st == ElementState::Pressed {
                                state.back_to_menu(&window, "back to menu");
                                window.request_redraw();
                                return;
                            }
                            if !state.kvm {
                                return;
                            }
                            if let Some((vk, ext)) = vkmap::to_vk(code) {
                                let down = st == ElementState::Pressed;
                                if let Some(s) = &state.session {
                                    let _ = s.cmd.send(ClientCommand::Key {
                                        vk,
                                        down,
                                        extended: ext,
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            },
            Event::AboutToWait => {
                state.drain_events(&window);
                state.drain_decoded(&window);
                if matches!(state.mode, Mode::Menu) {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    });
    if let Err(e) = result {
        log_err(&format!("event loop error: {}", e));
    }
}
