//! spacedesk-viewer: native high-performance spacedesk client for Linux
//! (developed and tested for Ubuntu 24.04).
//!
//! Pipeline: WebSocket (spacedesk protocol v4.8) -> FFmpeg decode (VA-API hw
//! accel when available) -> OpenGL texture upload -> GPU YUV->RGB.
//! Input (mouse/keyboard) is sent back as spacedesk KVM packets.
//!
//! Usage: spacedesk-viewer <server-ip> [options]   (see --help)

mod gl;
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
use spacedesk_core::protocol::{ClientType, Compression, CursorBitmapPacket, MouseButton, Transport};
use spacedesk_core::session::{run, standard_ladder, ClientCommand, ClientConfig, ServerEvent};
use std::num::NonZeroU32;
use std::sync::mpsc::{self, Receiver};
use winit::event::{ElementState, Event, KeyEvent, MouseButton as WinitMouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Fullscreen, Window, WindowBuilder};

struct Args {
    host: String,
    port: u16,
    compression: u32,
    quality: u32,
    subsampling: u32,
    frame_rate: u16,
    custom_res: Option<(u32, u32)>,
    win_size: (u32, u32),
    fullscreen: bool,
    kvm: bool,
    hwaccel: HwAccel,
    no_reconnect: bool,
    verbose: bool,
    client_type: u32,
    transport: Transport,
}

fn help() -> &'static str {
    "spacedesk-viewer <host> [options]\n\
     \nOptions:\n\
     \x20 --port N            server port (default 28252)\n\
     \x20 --compression C     h264 | jpeg | off (default h264)\n\
     \x20 --quality N         1-100 (default 70)\n\
     \x20 --subsampling S     0=4:4:4 1=4:2:2 2=4:2:0 (default 2)\n\
     \x20 --fps F             30 | 60 (default 30)\n\
     \x20 --resolution WxH    custom resolution, or 'client' (default client)\n\
     \x20 --size WxH          initial window size (default 1280x720)\n\
     \x20 --fullscreen        start fullscreen\n\
     \x20 --no-kvm            disable mouse/keyboard control\n\
     \x20 --hwaccel A         auto | vaapi | none (default auto)\n\
     \x20 --no-reconnect      do not reconnect on connection loss\n"
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut host = None;
    let mut port = spacedesk_core::protocol::DEFAULT_PORT;
    let mut compression = Compression::H264 as u32;
    let mut quality = 70u32;
    let mut subsampling = 2u32;
    let mut frame_rate = 0u16;
    let mut custom_res = None;
    let mut win_size = (1280u32, 720u32);
    let mut fullscreen = false;
    let mut kvm = true;
    let mut hwaccel = HwAccel::Auto;
    let mut no_reconnect = false;
    let mut verbose = false;
    let mut client_type = ClientType::WindowsRemoteMonitor as u32;
    let mut transport = Transport::Tcp;
    while let Some(a) = args.next() {
        let mut next = |name: &str| args.next().ok_or_else(|| format!("missing value for {}", name));
        match a.as_str() {
            h if host.is_none() && !h.starts_with('-') => host = Some(h.to_string()),
            "--port" => port = next("--port")?.parse().map_err(|_| "bad --port")?,
            "--compression" => {
                compression = match next("--compression")?.as_str() {
                    "h264" => Compression::H264 as u32,
                    "jpeg" => Compression::Jpeg as u32,
                    "off" | "raw" => Compression::Off as u32,
                    o => return Err(format!("unknown compression {}", o)),
                }
            }
            "--quality" => quality = next("--quality")?.parse().map_err(|_| "bad --quality")?,
            "--subsampling" => subsampling = next("--subsampling")?.parse().map_err(|_| "bad --subsampling")?,
            "--fps" => {
                frame_rate = match next("--fps")?.as_str() {
                    "30" => 0,
                    "60" => 1,
                    o => return Err(format!("unsupported fps {} (30 or 60)", o)),
                }
            }
            "--resolution" => {
                let v = next("--resolution")?;
                if v != "client" {
                    let p: Vec<&str> = v.split('x').collect();
                    if p.len() != 2 {
                        return Err("resolution must be WxH or 'client'".into());
                    }
                    custom_res = Some((p[0].parse().map_err(|_| "bad width")? , p[1].parse().map_err(|_| "bad height")?));
                }
            }
            "--size" => {
                let v = next("--size")?;
                let p: Vec<&str> = v.split('x').collect();
                if p.len() != 2 {
                    return Err("size must be WxH".into());
                }
                win_size = (p[0].parse().map_err(|_| "bad width")? , p[1].parse().map_err(|_| "bad height")?);
            }
            "--fullscreen" => fullscreen = true,
            "--no-kvm" => kvm = false,
            "--hwaccel" => {
                hwaccel = match next("--hwaccel")?.as_str() {
                    "auto" => HwAccel::Auto,
                    "vaapi" => HwAccel::Vaapi("/dev/dri/renderD128".to_string()),
                    "none" => HwAccel::None,
                    o => return Err(format!("unknown hwaccel {}", o)),
                }
            }
            "--no-reconnect" => no_reconnect = true,
            "--verbose" | "-v" => verbose = true,
            "--client-type" => {
                client_type = next("--client-type")?.parse().map_err(|_| "bad --client-type")?
            }
            "--transport" => {
                transport = match next("--transport")?.as_str() {
                    "tcp" => Transport::Tcp,
                    "ws" => Transport::Ws,
                    o => return Err(format!("unknown transport {}", o)),
                }
            }
            "--help" | "-h" => {
                println!("{}", help());
                std::process::exit(0);
            }
            o => return Err(format!("unknown argument {}\n\n{}", o, help())),
        }
    }
    let host = host
        .or_else(|| std::env::var("SPACEDESK_HOST").ok())
        .ok_or_else(|| format!("missing server host\n\n{}", help()))?;
    Ok(Args {
        host, port, compression, quality, subsampling, frame_rate, custom_res,
        win_size, fullscreen, kvm, hwaccel, no_reconnect, verbose, client_type, transport,
    })
}

enum ActiveDecoder {
    Raw(RawDecoder),
    Ffmpeg(FfmpegDecoder),
}

struct AppState {
    decoder: Option<ActiveDecoder>,
    pending_tiles: Vec<DecodedFrame>,
    screen: (u32, u32),
    cursor: Option<CursorImage>,
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    events: Receiver<ServerEvent>,
    commands: mpsc::Sender<ClientCommand>,
    kvm: bool,
    hwaccel: HwAccel,
    frame_rate: u16,
    last_mouse: Option<(f64, f64)>,
    grabbed: bool,
}

impl AppState {
    fn handle_event(&mut self, ev: ServerEvent, window: &Window) {
        match ev {
            ServerEvent::Connected => window.set_title("spacedesk viewer - connected"),
            ServerEvent::Error(e) => {
                eprintln!("error: {}", e);
                window.set_title(&format!("spacedesk viewer - error: {}", e));
            }
            ServerEvent::Disconnected { reason } => {
                eprintln!("disconnected: {}", reason);
                window.set_title(&format!("spacedesk viewer - disconnected: {}", reason));
            }
            ServerEvent::Visibility(_) | ServerEvent::PowerSuspendHibernate(_) | ServerEvent::Rotation(_) => {}
            ServerEvent::CursorPosition { x, y, visible } => {
                self.cursor_pos = (x, y);
                self.cursor_visible = visible;
            }
            ServerEvent::CursorBitmap(bmp) => {
                self.cursor = Some(cursor_from_packet(&bmp));
            }
            ServerEvent::Frame(p) => {
                // native TCP mode: header = full screen size, rect = tile bounds.
                // WS viewer mode: header = tile size, rect = dest position.
                let sw = p.width.max(p.rect.x2.max(0) as u32).max(1);
                let sh = p.height.max(p.rect.y2.max(0) as u32).max(1);
                if sw * sh > self.screen.0 * self.screen.1 {
                    self.screen = (sw, sh);
                }
                let want = codec_for_compression(p.compression);
                let need_new = match (&self.decoder, want) {
                    (None, _) => true,
                    (Some(ActiveDecoder::Raw(_)), None) => false,
                    (Some(ActiveDecoder::Raw(_)), Some(_)) => true,
                    (Some(ActiveDecoder::Ffmpeg(_)), None) => true,
                    (Some(ActiveDecoder::Ffmpeg(f)), Some(c)) => f.codec_kind() != c,
                };
                if need_new {
                    self.decoder = match want {
                        None => Some(ActiveDecoder::Raw(RawDecoder)),
                        Some(codec) => {
                            let (tw, th) = p.tile_dims();
                            match FfmpegDecoder::new(
                                codec,
                                self.hwaccel.clone(),
                                tw,
                                th,
                                if self.frame_rate == 1 { 60 } else { 30 },
                            ) {
                                Ok(d) => Some(ActiveDecoder::Ffmpeg(d)),
                                Err(e) => {
                                    eprintln!("decoder init failed: {}", e);
                                    None
                                }
                            }
                        }
                    };
                }
                let frames = match &mut self.decoder {
                    Some(ActiveDecoder::Raw(d)) => d.submit(&p).unwrap_or_default(),
                    Some(ActiveDecoder::Ffmpeg(d)) => d.submit(&p).unwrap_or_default(),
                    None => Vec::new(),
                };
                if !frames.is_empty() {
                    self.pending_tiles.extend(frames);
                    window.request_redraw();
                }
            }
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

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };
    if args.compression != Compression::Off as u32 && !ffmpeg_available() {
        eprintln!("warning: ffmpeg not found; H264/JPEG decoding needs it (`sudo apt install ffmpeg`)");
    }

    let mut cfg = ClientConfig::default();
    cfg.host = args.host.clone();
    cfg.port = args.port;
    cfg.reconnect = !args.no_reconnect;
    cfg.verbose = args.verbose;
    let ladder = standard_ladder(args.transport, args.client_type, args.compression);
    cfg.identification = ladder[0].clone();
    cfg.identification.transport = args.transport;
    cfg.identification.compression = args.compression;
    cfg.identification.fps = if args.frame_rate == 1 { 60 } else { 30 };
    cfg.identification.client_type = args.client_type;
    cfg.identification.quality = args.quality;
    cfg.identification.subsampling = args.subsampling;
    cfg.identification.frame_rate = args.frame_rate;
    cfg.identification.resolution = args.win_size;
    cfg.identification.resolution_custom = args.custom_res;
    cfg.identification.client_name = format!("spacedesk-viewer {} (Linux)", env!("CARGO_PKG_VERSION"));
    cfg.fallback_identifications = ladder[1..].to_vec();

    let (ev_tx, ev_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>();
    std::thread::spawn(move || {
        let _ = run(&cfg, ev_tx, cmd_rx);
    });

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let window_builder = WindowBuilder::new()
        .with_title("spacedesk viewer")
        .with_inner_size(winit::dpi::LogicalSize::new(args.win_size.0, args.win_size.1));
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
    let gl_surface = unsafe { gl_config.display().create_window_surface(&gl_config, &attrs) }.unwrap();
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
        .set_swap_interval(&gl_context, glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap()))
        .unwrap();
    let gl = unsafe {
        glow::Context::from_loader_function(|s| {
            gl_config
                .display()
                .get_proc_address(&std::ffi::CString::new(s).unwrap()) as *const std::ffi::c_void
        })
    };
    let mut renderer = unsafe { Renderer::new(gl) };

    if args.fullscreen {
        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
    }
    if args.kvm {
        let _ = window.set_cursor_grab(CursorGrabMode::Confined);
        window.set_cursor_visible(false);
    }

    let mut state = AppState {
        decoder: None,
        pending_tiles: Vec::new(),
        screen: (1, 1),
        cursor: None,
        cursor_pos: (0, 0),
        cursor_visible: false,
        events: ev_rx,
        commands: cmd_tx,
        kvm: args.kvm,
        hwaccel: args.hwaccel,
        frame_rate: args.frame_rate,
        last_mouse: None,
        grabbed: args.kvm,
    };

    let result = event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Poll);
        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    let _ = state.commands.send(ClientCommand::Disconnect);
                    elwt.exit();
                }
                WindowEvent::RedrawRequested => {
                    let size = window.inner_size();
                    let viewport = (size.width.max(1), size.height.max(1));
                    unsafe {
                        if !state.pending_tiles.is_empty() {
                            for tile in state.pending_tiles.drain(..) {
                                renderer.composite_tile(&tile, state.screen);
                            }
                        }
                        if state.screen != (1, 1) {
                            let cursor = if state.cursor_visible {
                                state.cursor.as_ref().map(|c| (c, state.cursor_pos.0, state.cursor_pos.1))
                            } else {
                                None
                            };
                            renderer.present(viewport, cursor);
                        } else {
                            renderer.clear_screen();
                        }
                        let _ = gl_surface.swap_buffers(&gl_context);
                    }
                }
                WindowEvent::Resized(size) => {
                    if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                        gl_surface.resize(&gl_context, w, h);
                    }
                    window.request_redraw();
                }
                WindowEvent::CursorMoved { position, .. } => {
                    if state.kvm {
                        let inner = window.inner_size();
                        if let Some(last) = state.last_mouse {
                            let scale_x = state.screen.0 as f64 / inner.width.max(1) as f64;
                            let scale_y = state.screen.1 as f64 / inner.height.max(1) as f64;
                            let dx = ((position.x - last.0) * scale_x) as i32;
                            let dy = ((position.y - last.1) * scale_y) as i32;
                            if dx != 0 || dy != 0 {
                                let _ = state.commands.send(ClientCommand::MouseMove { dx, dy });
                            }
                        }
                        state.last_mouse = Some((position.x, position.y));
                    }
                }
                WindowEvent::MouseInput { state: st, button, .. } => {
                    if !state.kvm {
                        return;
                    }
                    let btn = match button {
                        WinitMouseButton::Left => MouseButton::Left,
                        WinitMouseButton::Right => MouseButton::Right,
                        WinitMouseButton::Middle => MouseButton::Middle,
                        _ => MouseButton::X,
                    };
                    let down = st == ElementState::Pressed;
                    let _ = state.commands.send(ClientCommand::MouseButton { button: btn, down });
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    if !state.kvm {
                        return;
                    }
                    let d = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 120.0) as i32,
                        MouseScrollDelta::PixelDelta(p) => p.y as i32,
                    };
                    if d != 0 {
                        let _ = state.commands.send(ClientCommand::MouseWheel { delta: d });
                    }
                }
                WindowEvent::KeyboardInput {
                    event: KeyEvent { state: st, physical_key: PhysicalKey::Code(code), .. },
                    ..
                } => {
                    if code == KeyCode::F11 && st == ElementState::Pressed {
                        let fs = window.fullscreen();
                        if fs.is_some() {
                            window.set_fullscreen(None);
                        } else {
                            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                        }
                        return;
                    }
                    if code == KeyCode::Escape && st == ElementState::Pressed && state.grabbed {
                        let _ = window.set_cursor_grab(CursorGrabMode::None);
                        window.set_cursor_visible(true);
                        state.grabbed = false;
                    }
                    if !state.kvm {
                        return;
                    }
                    if let Some((vk, ext)) = vkmap::to_vk(code) {
                        let down = st == ElementState::Pressed;
                        let _ = state.commands.send(ClientCommand::Key { vk, down, extended: ext });
                    }
                }
                _ => {}
            },
            Event::AboutToWait => {
                while let Ok(ev) = state.events.try_recv() {
                    state.handle_event(ev, &window);
                }
            }
            _ => {}
        }
    });
    if let Err(e) = result {
        eprintln!("event loop error: {}", e);
    }
}
