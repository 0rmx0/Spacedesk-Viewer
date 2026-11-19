//! spacedesk-cli: headless native spacedesk client.
//!
//! Connects to a spacedesk Windows server, decodes the video stream (H264 via
//! FFmpeg/VA-API, JPEG via FFmpeg, or raw) and prints statistics; can dump frames
//! as PPM images. Useful to validate the pipeline without a display.
//!
//! Example:
//!   spacedesk-cli 192.168.1.10 --compression h264 --fps 60 --frames 120 --dump /tmp/frames

use spacedesk_core::decoder::{Decoder, RawDecoder};
use spacedesk_core::ffmpeg::codec_for_compression;
use spacedesk_core::ffmpeg::{ffmpeg_available, FfmpegDecoder, HwAccel};
use spacedesk_core::frame::{write_ppm, DecodedFrame};
use spacedesk_core::protocol::{ClientType, Compression};
use spacedesk_core::protocol::Transport;
use spacedesk_core::session::{run, standard_ladder, ClientConfig, ServerEvent};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Args {
    host: String,
    port: u16,
    compression: u32,
    quality: u32,
    subsampling: u32,
    frame_rate: u16,
    custom_res: Option<(u32, u32)>,
    client_res: (u32, u32),
    frames: u64,
    duration: u64,
    dump: Option<PathBuf>,
    hwaccel: HwAccel,
    no_reconnect: bool,
    verbose: bool,
    client_type: u32,
    transport: Transport,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut host = None;
    let mut port = spacedesk_core::protocol::DEFAULT_PORT;
    let mut compression = Compression::H264 as u32;
    let mut quality = 70u32;
    let mut subsampling = 2u32;
    let mut frame_rate = 0u16; // 0 = 30fps, 1 = 60fps
    let mut custom_res = None;
    let mut frames = 0u64;
    let mut duration = 0u64;
    let mut dump = None;
    let mut hwaccel = HwAccel::Auto;
    let mut no_reconnect = false;
    let mut verbose = false;
    let mut client_type = ClientType::WindowsRemoteMonitor as u32;
    let mut transport = Transport::Tcp;

    while let Some(a) = args.next() {
        let mut next = |name: &str| -> Result<String, String> {
            args.next().ok_or_else(|| format!("missing value for {}", name))
        };
        match a.as_str() {
            h if host.is_none() && !h.starts_with('-') => host = Some(h.to_string()),
            "--port" => port = next("--port")?.parse().map_err(|_| "bad --port")?,
            "--compression" => {
                compression = match next("--compression")?.as_str() {
                    "h264" => Compression::H264 as u32,
                    "jpeg" => Compression::Jpeg as u32,
                    "off" | "raw" => Compression::Off as u32,
                    other => return Err(format!("unknown compression {}", other)),
                }
            }
            "--quality" => quality = next("--quality")?.parse().map_err(|_| "bad --quality")?,
            "--subsampling" => {
                subsampling = next("--subsampling")?
                    .parse()
                    .map_err(|_| "bad --subsampling")?
            }
            "--fps" => {
                frame_rate = match next("--fps")?.as_str() {
                    "30" => 0,
                    "60" => 1,
                    other => return Err(format!("unsupported fps {} (use 30 or 60)", other)),
                }
            }
            "--resolution" => {
                let v = next("--resolution")?;
                if v != "client" {
                    let parts: Vec<&str> = v.split('x').collect();
                    if parts.len() != 2 {
                        return Err("resolution must be WxH or 'client'".to_string());
                    }
                    custom_res = Some((
                        parts[0].parse().map_err(|_| "bad width")?,
                        parts[1].parse().map_err(|_| "bad height")?,
                    ));
                }
            }
            "--frames" => frames = next("--frames")?.parse().map_err(|_| "bad --frames")?,
            "--duration" => duration = next("--duration")?.parse().map_err(|_| "bad --duration")?,
            "--dump" => dump = Some(PathBuf::from(next("--dump")?)),
            "--hwaccel" => {
                hwaccel = match next("--hwaccel")?.as_str() {
                    "auto" => HwAccel::Auto,
                    "vaapi" => HwAccel::Vaapi("/dev/dri/renderD128".to_string()),
                    "none" => HwAccel::None,
                    other => return Err(format!("unknown hwaccel {}", other)),
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
            other => return Err(format!("unknown argument {}", other)),
        }
    }
    let host = host
        .or_else(|| std::env::var("SPACEDESK_HOST").ok())
        .ok_or_else(|| format!("missing server host\n\n{}", help()))?;
    Ok(Args {
        host,
        port,
        compression,
        quality,
        subsampling,
        frame_rate,
        custom_res,
        client_res: (1920, 1080),
        frames,
        duration,
        dump,
        hwaccel,
        no_reconnect,
        verbose,
        client_type,
        transport,
    })
}

fn help() -> &'static str {
    "spacedesk-cli <host> [options]\n\
     \nOptions:\n\
     \x20 --port N            server port (default 28252)\n\
     \x20 --compression C     h264 | jpeg | off (default h264)\n\
     \x20 --quality N         1-100 (default 70)\n\
     \x20 --subsampling S     0=4:4:4 1=4:2:2 2=4:2:0 (default 2)\n\
     \x20 --fps F             30 | 60 (default 30)\n\
     \x20 --resolution WxH    custom resolution, or 'client' (default client)\n\
     \x20 --frames N          exit after N decoded frames (0 = run forever)\n\
     \x20 --duration S        exit after S seconds (0 = unlimited)\n\
     \x20 --dump DIR          write decoded frames as PPM images into DIR\n\
     \x20 --hwaccel A         auto | vaapi | none (default auto)\n\
     \x20 --no-reconnect      do not reconnect on connection loss\n\
     \x20 -v, --verbose       verbose logging\n"
}

enum ActiveDecoder {
    Raw(RawDecoder),
    Ffmpeg(FfmpegDecoder),
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };

    if let Some(dir) = &args.dump {
        std::fs::create_dir_all(dir).expect("cannot create dump dir");
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
    cfg.identification.resolution = args.client_res;
    cfg.identification.resolution_custom = args.custom_res;
    cfg.identification.client_name = format!("spacedesk-cli {} (Linux)", env!("CARGO_PKG_VERSION"));
    cfg.fallback_identifications = ladder[1..].to_vec();

    eprintln!(
        "connecting to {}:{} client_type={} compression={} quality={} fps_mode={} custom_res={:?}",
        cfg.host, cfg.port, args.client_type, args.compression, args.quality, args.frame_rate, args.custom_res
    );
    if args.compression != Compression::Off as u32 && !ffmpeg_available() {
        eprintln!("warning: ffmpeg not found — install it (`sudo apt install ffmpeg`) for H264/JPEG decoding");
    }

    let (ev_tx, ev_rx) = mpsc::channel();
    let (_cmd_tx, cmd_rx) = mpsc::channel();
    let session = std::thread::spawn(move || run(&cfg, ev_tx, cmd_rx));

    let mut decoder: Option<ActiveDecoder> = None;
    let mut frames_done = 0u64;
    let mut bytes_in = 0u64;
    let start = Instant::now();
    let mut last_stats = Instant::now();
    let mut last_frame = Instant::now();

    loop {
        if args.duration > 0 && start.elapsed().as_secs() >= args.duration {
            eprintln!("duration reached, exiting");
            break;
        }
        let ev = match ev_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ev) => ev,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if session.is_finished() {
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match ev {
            ServerEvent::Connected => eprintln!("connected"),
            ServerEvent::Error(e) => eprintln!("error: {}", e),
            ServerEvent::Disconnected { reason } => {
                eprintln!("disconnected: {}", reason);
                break;
            }
            ServerEvent::Visibility(v) => {
                if args.verbose {
                    eprintln!("visibility: {}", v);
                }
            }
            ServerEvent::CursorPosition { x, y, visible } => {
                if args.verbose {
                    eprintln!("cursor: {} {} visible={}", x, y, visible);
                }
            }
            ServerEvent::CursorBitmap(_) => {}
            ServerEvent::PowerSuspendHibernate(v) => eprintln!("power event: {}", v),
            ServerEvent::Rotation(v) => eprintln!("rotation: {}", v),
            ServerEvent::Frame(p) => {
                bytes_in += p.payload.len() as u64;
                let want_codec = codec_for_compression(p.compression);
                let need_new = match (&decoder, want_codec) {
                    (None, _) => true,
                    (Some(ActiveDecoder::Raw(_)), None) => false,
                    (Some(ActiveDecoder::Raw(_)), Some(_)) => true,
                    (Some(ActiveDecoder::Ffmpeg(_)), None) => true,
                    (Some(ActiveDecoder::Ffmpeg(_)), Some(c)) => {
                        // recreate when codec kind changes; FfmpegDecoder respawns on size change
                        let cur = match &decoder {
                            Some(ActiveDecoder::Ffmpeg(f)) => Some(f.codec_kind()),
                            _ => None,
                        };
                        cur != Some(c)
                    }
                };
                if need_new {
                    decoder = match want_codec {
                        None => Some(ActiveDecoder::Raw(RawDecoder)),
                        Some(codec) => {
                            let (tw, th) = p.tile_dims();
                            match FfmpegDecoder::new(
                                codec,
                                args.hwaccel.clone(),
                                tw,
                                th,
                                if args.frame_rate == 1 { 60 } else { 30 },
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
                let decoded: Vec<DecodedFrame> = match &mut decoder {
                    Some(ActiveDecoder::Raw(d)) => match d.submit(&p) {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("raw decode: {}", e);
                            continue;
                        }
                    },
                    Some(ActiveDecoder::Ffmpeg(d)) => match d.submit(&p) {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("decode: {}", e);
                            continue;
                        }
                    },
                    None => continue,
                };
                for frame in decoded {
                    frames_done += 1;
                    let now = Instant::now();
                    let dt = now.duration_since(last_frame).as_secs_f64();
                    last_frame = now;
                    if args.verbose {
                        eprintln!(
                            "frame #{} {}x{} {:?} ({:.1} fps instantaneous)",
                            frames_done,
                            frame.width,
                            frame.height,
                            frame.format,
                            if dt > 0.0 { 1.0 / dt } else { 0.0 }
                        );
                    }
                    if let Some(dir) = &args.dump {
                        let path = dir.join(format!("frame-{:06}.ppm", frames_done));
                        if let Err(e) = write_ppm(&path, &frame) {
                            eprintln!("dump failed: {}", e);
                        }
                    }
                    if args.frames > 0 && frames_done >= args.frames {
                        let total = start.elapsed().as_secs_f64();
                        eprintln!(
                            "done: {} frames in {:.2}s ({:.1} fps avg, {:.1} Mbit/s in)",
                            frames_done,
                            total,
                            frames_done as f64 / total.max(0.001),
                            bytes_in as f64 * 8.0 / total.max(0.001) / 1e6
                        );
                        std::process::exit(0);
                    }
                }
            }
        }
        if last_stats.elapsed().as_secs() >= 2 {
            let el = start.elapsed().as_secs_f64();
            eprintln!(
                "stats: {} frames, {:.1} fps avg, {:.2} Mbit/s in, {:.1}s uptime",
                frames_done,
                frames_done as f64 / el.max(0.001),
                bytes_in as f64 * 8.0 / el.max(0.001) / 1e6,
                el
            );
            last_stats = Instant::now();
        }
    }
    let _ = session.join();
}
