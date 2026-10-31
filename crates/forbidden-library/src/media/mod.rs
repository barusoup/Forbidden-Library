//! Playback engines for the viewer: video with optional audio, and animated
//! images. Nothing here touches the UI or the GPU. The viewer polls these
//! engines from the event loop, uploads the frames they hand back, and passes
//! in a `Wake` closure so a background thread can ask for a poll when new data
//! lands.

pub mod anim;
pub mod audio;
pub mod clock;
pub mod video;

#[cfg(test)]
mod matrix;

use std::sync::Arc;

/// Called from a background thread when the engine has something new to hand
/// over. The app passes a closure that pokes the event loop.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// One decoded frame of RGBA8 pixels, ready for upload.
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
