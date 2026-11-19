//! Decoder abstraction: turns wire frame payloads into [`DecodedFrame`]s.

use crate::frame::{DecodedFrame, PixelFormat};
use crate::protocol::{Compression, FrameBufferPacket};

#[derive(Debug)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

pub trait Decoder {
    /// Submit one (reassembled) frame payload; returns any frames that became
    /// available (a decoder may buffer or emit several frames per submit).
    fn submit(&mut self, packet: &FrameBufferPacket) -> Result<Vec<DecodedFrame>, DecodeError>;
}

/// Decoder for uncompressed frames (`compression = Off`).
/// The wire payload of RGBX32/RGBA32 frames is BGRA byte order (see the reference
/// viewer's raw path, which swaps R and B); YUV frames are passed through as NV12
/// when the subsampling is 4:2:0.
pub struct RawDecoder;

impl Decoder for RawDecoder {
    fn submit(&mut self, packet: &FrameBufferPacket) -> Result<Vec<DecodedFrame>, DecodeError> {
        let format = match packet.format {
            3 | 4 => PixelFormat::Bgra, // RGBX32 / RGBA32 (BGRA on the wire)
            7 => PixelFormat::Nv12,     // YUV420
            other => {
                return Err(DecodeError(format!(
                    "unsupported raw color format {}",
                    other
                )))
            }
        };
        let (tw, th) = packet.tile_dims();
        let expected = format.bytes_per_frame(tw, th);
        if packet.payload.len() < expected {
            return Err(DecodeError(format!(
                "raw frame too small: {} < {} bytes",
                packet.payload.len(),
                expected
            )));
        }
        Ok(vec![DecodedFrame {
            width: tw,
            height: th,
            format,
            rect: packet.rect,
            data: packet.payload[..expected].to_vec(),
        }])
    }
}

/// Helper: is this payload a self-contained JPEG (starts with SOI, ends with EOI)?
pub fn is_complete_jpeg(data: &[u8]) -> bool {
    data.len() >= 4
        && data[0] == 0xFF
        && data[1] == 0xD8
        && data[data.len() - 2] == 0xFF
        && data[data.len() - 1] == 0xD9
}

pub fn compression_kind(compression: u32) -> Option<Compression> {
    Compression::from_u32(compression)
}
