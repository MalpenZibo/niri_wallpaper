//! Cross-fade animation between wallpapers.
//!
//! The fade pre-scales old and new images per output at animation start,
//! then blends them per-frame using integer alpha. Only Normal surfaces are
//! animated; Backdrop surfaces are instant-swapped.

use std::time::{Duration, Instant};

use image::RgbaImage;

use crate::config::EasingKind;

/// Per-output pre-scaled frames for the cross-fade.
pub struct OutputFadeFrames {
    pub old_normal: RgbaImage,
    pub new_normal: RgbaImage,
}

/// Active cross-fade state.
pub struct FadeState {
    pub start: Instant,
    pub duration: Duration,
    pub easing: EasingKind,
    /// Per output index. `None` if the output has no configured size yet.
    pub output_frames: Vec<Option<OutputFadeFrames>>,
}

impl FadeState {
    /// Compute the eased progress in [0, 1].
    pub fn progress(&self) -> f32 {
        let elapsed = self.start.elapsed().as_secs_f32();
        let total = self.duration.as_secs_f32();
        let t = (elapsed / total).clamp(0.0, 1.0);
        self.easing.apply(t)
    }

    /// Returns `true` if the animation has finished.
    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }
}

/// Alpha-blend two same-sized images using fixed-point integer arithmetic.
///
/// `alpha` is in [0, 1]: 0 = fully `old`, 1 = fully `new`.
pub fn blend(old: &RgbaImage, new: &RgbaImage, alpha: f32) -> RgbaImage {
    debug_assert_eq!(old.dimensions(), new.dimensions());
    let (w, h) = old.dimensions();

    // Fixed-point: alpha_i in [0, 256].
    let alpha_i = (alpha * 256.0).round() as u16;
    let inv_i = 256 - alpha_i;

    let old_raw = old.as_raw();
    let new_raw = new.as_raw();
    let len = old_raw.len();
    let mut out = vec![0u8; len];

    // Process 4 bytes at a time — the compiler auto-vectorises this loop
    // on x86_64 with SIMD when built with opt-level ≥ 2.
    for i in 0..len {
        out[i] = ((old_raw[i] as u16 * inv_i + new_raw[i] as u16 * alpha_i) >> 8) as u8;
    }

    RgbaImage::from_raw(w, h, out).expect("blend: buffer matches dims")
}
