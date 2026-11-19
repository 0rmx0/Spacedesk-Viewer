//! In-process JPEG decoder for spacedesk JPEG/MJPEG tiles (`compression = 3`).
//!
//! The driver sends one complete JPEG per FrameBuffer packet (the tile bounds
//! are in the packet rect); decoding in-process with `jpeg-decoder` avoids the
//! ffmpeg-pipe buffering latency entirely and handles varying tile sizes
//! natively. Output is converted to BGRA, which is what the GL renderer uploads.

use crate::decoder::{is_complete_jpeg, DecodeError, Decoder};
use crate::frame::{DecodedFrame, PixelFormat};
use crate::protocol::FrameBufferPacket;

pub struct JpegDecoder;

impl Decoder for JpegDecoder {
    fn submit(&mut self, packet: &FrameBufferPacket) -> Result<Vec<DecodedFrame>, DecodeError> {
        if packet.payload.is_empty() || !is_complete_jpeg(&packet.payload) {
            // not a self-contained JPEG: the session reassembles fragments
            return Ok(Vec::new());
        }
        let mut dec = jpeg_decoder::Decoder::new(&packet.payload[..]);
        dec.read_info()
            .map_err(|e| DecodeError(format!("jpeg header: {}", e)))?;
        let info = dec
            .info()
            .ok_or_else(|| DecodeError("jpeg: no image info".to_string()))?;
        let (w, h) = (info.width as u32, info.height as u32);
        let decoded = dec
            .decode()
            .map_err(|e| DecodeError(format!("jpeg decode: {}", e)))?;
        let px = (w as usize) * (h as usize);
        if decoded.len() < px * info.pixel_format.pixel_bytes() {
            return Err(DecodeError(format!(
                "jpeg: short pixel buffer {} < {}",
                decoded.len(),
                px * info.pixel_format.pixel_bytes()
            )));
        }
        // convert to BGRA (the renderer's upload format)
        let mut bgra = vec![0u8; px * 4];
        match info.pixel_format {
            jpeg_decoder::PixelFormat::RGB24 => {
                for i in 0..px {
                    bgra[i * 4] = decoded[i * 3 + 2]; // B
                    bgra[i * 4 + 1] = decoded[i * 3 + 1]; // G
                    bgra[i * 4 + 2] = decoded[i * 3]; // R
                    bgra[i * 4 + 3] = 255;
                }
            }
            jpeg_decoder::PixelFormat::L8 => {
                for i in 0..px {
                    let v = decoded[i];
                    bgra[i * 4] = v;
                    bgra[i * 4 + 1] = v;
                    bgra[i * 4 + 2] = v;
                    bgra[i * 4 + 3] = 255;
                }
            }
            other => {
                return Err(DecodeError(format!(
                    "jpeg: unsupported pixel format {:?}",
                    other
                )))
            }
        }
        Ok(vec![DecodedFrame {
            width: w,
            height: h,
            format: PixelFormat::Bgra,
            rect: packet.rect,
            data: bgra,
        }])
    }
}
