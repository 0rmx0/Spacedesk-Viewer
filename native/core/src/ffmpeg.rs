//! FFmpeg-based hardware/software decoder.
//!
//! Instead of FFI bindings (fragile across FFmpeg versions), we spawn the `ffmpeg`
//! CLI as a pipe: H264/MJPEG bytes go to stdin, packed NV12 raw frames come out of
//! stdout. On Ubuntu 24.04 with Intel/AMD graphics this uses VA-API hardware decoding
//! (`-hwaccel vaapi`), falling back to software automatically when no GPU device or
//! no `ffmpeg` binary is available (the caller should then fall back to JPEG/raw).

use crate::decoder::{DecodeError, Decoder};
use crate::frame::{DecodedFrame, PixelFormat};
use crate::protocol::Rect;
use crate::protocol::{Compression, FrameBufferPacket};
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    H264,
    Mjpeg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwAccel {
    /// Pick VA-API if a render node exists, otherwise software.
    Auto,
    Vaapi(String),
    None,
}

impl HwAccel {
    pub fn resolve(&self) -> HwAccel {
        match self {
            HwAccel::Auto => {
                for dev in ["/dev/dri/renderD128", "/dev/dri/renderD129"] {
                    if std::path::Path::new(dev).exists() {
                        return HwAccel::Vaapi(dev.to_string());
                    }
                }
                HwAccel::None
            }
            other => other.clone(),
        }
    }
}

pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct FfmpegDecoder {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<io::Result<DecodedFrame>>,
    /// Sizes of the frames ffmpeg is expected to output, in submission order.
    /// MJPEG tiles vary in size packet to packet; the H264 stream has a fixed
    /// resolution (the packet header's width/height).
    expect_tx: Option<Sender<(u32, u32, Rect)>>,
    reader: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
    codec: CodecKind,
    hwaccel: HwAccel,
    frame_rate: u32,
    tried_software: bool,
}

impl FfmpegDecoder {
    pub fn new(
        codec: CodecKind,
        hwaccel: HwAccel,
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Result<FfmpegDecoder, DecodeError> {
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-fflags")
            .arg("+genpts")
            .arg("-flags")
            .arg("low_delay");
        match hwaccel.resolve() {
            HwAccel::Vaapi(dev) => {
                cmd.arg("-hwaccel")
                    .arg("vaapi")
                    .arg("-hwaccel_device")
                    .arg(dev);
            }
            HwAccel::None => {
                cmd.arg("-hwaccel").arg("none");
            }
            HwAccel::Auto => unreachable!(),
        }
        let input_format = match codec {
            CodecKind::H264 => "h264",
            CodecKind::Mjpeg => "mjpeg",
        };
        cmd.arg("-f")
            .arg(input_format)
            .arg("-framerate")
            .arg(frame_rate.max(1).to_string())
            .arg("-i")
            .arg("pipe:0")
            .arg("-an")
            .arg("-sn")
            .arg("-dn")
            .arg("-f")
            .arg("rawvideo")
            // BGRA on purpose: the reader consumes width*height*4 bytes per
            // frame and the GL renderer uploads BGRA directly (no NV12 shader).
            .arg("-pix_fmt")
            .arg("bgra")
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            DecodeError(format!(
                "failed to spawn ffmpeg (install it with `sudo apt install ffmpeg`): {}",
                e
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| DecodeError("ffmpeg stdin unavailable".to_string()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| DecodeError("ffmpeg stdout unavailable".to_string()))?;

        let (tx, rx) = mpsc::channel();
        let (expect_tx, expect_rx) = mpsc::channel::<(u32, u32, Rect)>();
        let reader = std::thread::spawn(move || {
            loop {
                let (w, h, rect) = match expect_rx.recv() {
                    Ok(v) => v,
                    Err(_) => break, // decoder shut down
                };
                let size = PixelFormat::Bgra.bytes_per_frame(w, h);
                let mut buf = vec![0u8; size];
                match stdout.read_exact(&mut buf) {
                    Ok(()) => {
                        if tx
                            .send(Ok(DecodedFrame {
                                width: w,
                                height: h,
                                format: PixelFormat::Bgra,
                                rect,
                                data: buf,
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        Ok(FfmpegDecoder {
            child,
            stdin: Some(stdin),
            rx,
            expect_tx: Some(expect_tx),
            reader: Some(reader),
            width,
            height,
            codec,
            hwaccel,
            frame_rate,
            tried_software: false,
        })
    }

    fn respawn(&mut self, width: u32, height: u32, frame_rate: u32, hwaccel: HwAccel) -> Result<(), DecodeError> {
        self.shutdown();
        *self = FfmpegDecoder::new(self.codec, hwaccel, width, height, frame_rate)?;
        Ok(())
    }

    fn shutdown(&mut self) {
        // closing stdin signals EOF to ffmpeg; then make sure the process exits
        drop(self.stdin.take());
        // dropping expect_tx makes the reader thread's recv() fail so it exits
        drop(self.expect_tx.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }

    pub fn codec_kind(&self) -> CodecKind {
        self.codec
    }

    fn write_payload(&mut self, payload: &[u8], expect: (u32, u32, Rect)) -> Result<(), DecodeError> {
        // If the hw-accelerated ffmpeg died (e.g. VAAPI unusable on this
        // machine), retry once in software before giving up.
        for attempt in 0..2 {
            if attempt == 1 {
                if self.tried_software {
                    return Err(DecodeError("ffmpeg pipe write failed".to_string()));
                }
                self.tried_software = true;
                self.respawn(self.width, self.height, self.frame_rate, HwAccel::None)?;
            }
            let tx = self
                .expect_tx
                .as_ref()
                .ok_or_else(|| DecodeError("ffmpeg reader gone".to_string()))?;
            if tx.send(expect).is_err() {
                continue;
            }
            let stdin = self
                .stdin
                .as_mut()
                .ok_or_else(|| DecodeError("ffmpeg stdin closed".to_string()))?;
            match stdin.write_all(payload).and_then(|_| stdin.flush()) {
                Ok(()) => return Ok(()),
                Err(_) => continue,
            }
        }
        Err(DecodeError("ffmpeg pipe write failed".to_string()))
    }
}

impl Decoder for FfmpegDecoder {
    fn submit(&mut self, packet: &FrameBufferPacket) -> Result<Vec<DecodedFrame>, DecodeError> {
        // H264: the stream has the resolution from the packet header (full
        // screen in native TCP mode); rect only marks the dirty region.
        // MJPEG: every packet is a self-contained JPEG of the tile size.
        let (tw, th, rect) = match self.codec {
            CodecKind::H264 => {
                let hw = packet.width.max(1);
                let hh = packet.height.max(1);
                if (hw, hh) != (self.width, self.height) {
                    // stream resolution changed (rotation / resolution change)
                    self.respawn(hw, hh, self.frame_rate, self.hwaccel.clone())?;
                }
                (
                    self.width,
                    self.height,
                    Rect { x: 0, y: 0, x2: self.width as i32, y2: self.height as i32 },
                )
            }
            CodecKind::Mjpeg => {
                let (tw, th) = packet.tile_dims();
                (tw, th, packet.rect)
            }
        };
        if !packet.payload.is_empty() {
            self.write_payload(&packet.payload, (tw, th, rect))?;
        }
        let mut frames = Vec::new();
        while let Ok(res) = self.rx.try_recv() {
            match res {
                Ok(f) => frames.push(f),
                Err(e) => return Err(DecodeError(format!("ffmpeg read: {}", e))),
            }
        }
        Ok(frames)
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Pick the right decoder kind for a wire compression value.
pub fn codec_for_compression(compression: u32) -> Option<CodecKind> {
    match Compression::from_u32(compression) {
        Some(Compression::H264) => Some(CodecKind::H264),
        Some(Compression::Jpeg) | Some(Compression::MjpegD2) => Some(CodecKind::Mjpeg),
        Some(Compression::Off) | None => None,
    }
}
