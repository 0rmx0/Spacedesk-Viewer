//! spacedesk-core: protocol, transport, session and decoding for a native,
//! high-performance spacedesk viewer on Linux (developed for Ubuntu 24.04).
//!
//! Layers:
//! - [`protocol`]: the spacedesk binary protocol (v4.8) extracted from the official
//!   HTML5 viewer bundled in this repository.
//! - [`ws`]: minimal RFC 6455 WebSocket client (std only).
//! - [`session`]: connection/handshake/fragment-reassembly/flow-control/KVM.
//! - [`decoder`] / [`ffmpeg`]: raw and FFmpeg (VA-API hw accel) decoders.

pub mod decoder;
pub mod ffmpeg;
pub mod frame;
pub mod protocol;
pub mod session;
pub mod transport;
pub mod ws;
