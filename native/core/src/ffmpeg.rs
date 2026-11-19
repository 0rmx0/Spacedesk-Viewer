//! FFmpeg-based hardware/software decoder.
//!
//! Instead of FFI bindings (fragile across FFmpeg versions), we spawn the `ffmpeg`
//! CLI as a pipe: H264/MJPEG bytes go to stdin, packed NV12 raw frames come out of
//! stdout. On Ubuntu 24.04 with Intel/AMD graphics this uses VA-API hardware decoding
//! (`-hwaccel vaapi`), falling back to software automatically when no GPU device or
//! no `ffmpeg` binary is available (the caller should then fall back to JPEG/raw).

use crate::decoder::{DecodeError, Decoder};
use crate::frame::{DecodedFrame, PixelFormat};
use crate::protocol::{Compression, FrameBufferPacket};
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
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
    reader: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
    codec: CodecKind,
}

impl FfmpegDecoder {
    pub fn new(
        codec: CodecKind,
        hwaccel: HwAccel,
        width: u32,
        height: u32,
        frame_rate: u32,
    ) -> Result<FfmpegDecoder, DecodeError> {
        let frame_size = PixelFormat::Nv12.bytes_per_frame(width, height);
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
                    .arg(dev)
                    .arg("-hwaccel_output_format")
                    .arg("nv12");
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
            .arg("-pix_fmt")
            .arg("nv12")
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
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0u8; frame_size];
            loop {
                match stdout.read_exact(&mut buf) {
                    Ok(()) => {
                        if tx
                            .send(Ok(DecodedFrame {
                                width,
                                height,
                                format: PixelFormat::Nv12,
                                data: buf.clone(),
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
            reader: Some(reader),
            width,
            height,
            codec,
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
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

impl FfmpegDecoder {
    pub fn codec_kind(&self) -> CodecKind {
        self.codec
    }
}

impl Decoder for FfmpegDecoder {
    fn submit(&mut self, packet: &FrameBufferPacket) -> Result<Vec<DecodedFrame>, DecodeError> {
        if packet.width != self.width || packet.height != self.height {
            // resolution changed (rotation / display change): restart the pipe
            self.respawn(packet.width, packet.height, 60, HwAccel::Auto)?;
        }
        if packet.payload.is_empty() {
            return Ok(Vec::new());
        }
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| DecodeError("ffmpeg stdin closed".to_string()))?;
        stdin
            .write_all(&packet.payload)
            .map_err(|e| DecodeError(format!("ffmpeg pipe write failed: {}", e)))?;
        stdin
            .flush()
            .map_err(|e| DecodeError(format!("ffmpeg pipe flush failed: {}", e)))?;
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
