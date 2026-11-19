//! Decoded frame representation shared by decoders and renderers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Packed NV12: Y plane (w*h) then interleaved UV plane (2*ceil(w/2)*ceil(h/2)).
    Nv12,
    /// Packed BGRA (as sent on the wire for uncompressed frames).
    Bgra,
    /// Packed RGBA.
    Rgba,
}

impl PixelFormat {
    pub fn bytes_per_frame(&self, w: u32, h: u32) -> usize {
        match self {
            PixelFormat::Nv12 => {
                let y = (w as usize) * (h as usize);
                let uv = 2 * (((w as usize) + 1) / 2) * (((h as usize) + 1) / 2);
                y + uv
            }
            PixelFormat::Bgra | PixelFormat::Rgba => (w as usize) * (h as usize) * 4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Packed pixel data (`format.bytes_per_frame(width, height)` bytes).
    pub data: Vec<u8>,
}

/// Convert an NV12 frame to packed RGB24 (used for dumping frames as PPM).
pub fn nv12_to_rgb24(frame: &DecodedFrame) -> Vec<u8> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let y_plane = &frame.data[..w * h];
    let uv_plane = &frame.data[w * h..];
    let uv_stride = 2 * ((w + 1) / 2);
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            let yv = y_plane[y * w + x] as f32;
            let u = uv_plane[(y / 2) * uv_stride + (x / 2) * 2] as f32 - 128.0;
            let v = uv_plane[(y / 2) * uv_stride + (x / 2) * 2 + 1] as f32 - 128.0;
            let r = yv + 1.402 * v;
            let g = yv - 0.344136 * u - 0.714136 * v;
            let b = yv + 1.772 * u;
            let o = (y * w + x) * 3;
            out[o] = r.clamp(0.0, 255.0) as u8;
            out[o + 1] = g.clamp(0.0, 255.0) as u8;
            out[o + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Convert a BGRA frame to packed RGB24.
pub fn bgra_to_rgb24(frame: &DecodedFrame) -> Vec<u8> {
    let px = frame.width as usize * frame.height as usize;
    let mut out = vec![0u8; px * 3];
    for i in 0..px {
        out[i * 3] = frame.data[i * 4 + 2];
        out[i * 3 + 1] = frame.data[i * 4 + 1];
        out[i * 3 + 2] = frame.data[i * 4];
    }
    out
}

/// Write a decoded frame as a binary PPM (P6) file.
pub fn write_ppm(path: &std::path::Path, frame: &DecodedFrame) -> std::io::Result<()> {
    let rgb = match frame.format {
        PixelFormat::Nv12 => nv12_to_rgb24(frame),
        PixelFormat::Bgra => bgra_to_rgb24(frame),
        PixelFormat::Rgba => {
            let px = frame.width as usize * frame.height as usize;
            let mut out = vec![0u8; px * 3];
            for i in 0..px {
                out[i * 3] = frame.data[i * 4];
                out[i * 3 + 1] = frame.data[i * 4 + 1];
                out[i * 3 + 2] = frame.data[i * 4 + 2];
            }
            out
        }
    };
    let mut file = std::fs::File::create(path)?;
    use std::io::Write;
    writeln!(file, "P6\n{} {}\n255", frame.width, frame.height)?;
    file.write_all(&rgb)
}
