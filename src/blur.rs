//! Pure-CPU blur used for the overview backdrop surface.
//!
//! Strategy:
//!   1. Iteratively downsample by 2× (simple 2×2 box average) until the image
//!      reaches roughly `1/downsample` of its original area. Downsampling does
//!      the bulk of the blurring "for free" and keeps the subsequent passes on
//!      a tiny image.
//!   2. Apply three iterations of a horizontal + vertical box blur. Three
//!      iterations of a box filter is a well-known, visually close
//!      approximation of a true Gaussian blur and runs in O(n) per pass
//!      regardless of radius thanks to a running-sum.
//!
//! This runs exactly once per wallpaper change (≈ daily). On a modern CPU a
//! 4K source is processed in well under 100 ms and the process then returns
//! to being fully idle — no persistent GPU context, no wake-ups.

use image::RgbaImage;

/// Blur `src` into a small, cover-stretchable preview suitable for the backdrop.
///
/// * `downsample` — target downscale factor (≥ 1). Each output dimension will
///   be at most `src_dim / downsample`.
/// * `radius` — box blur radius in downsampled pixels. Effective Gaussian
///   sigma ≈ `radius * sqrt(iterations / 3)`.
/// * `iterations` — number of box-blur passes. 3 yields a smooth Gaussian-like
///   result; higher values approach a true Gaussian at extra cost.
pub fn blur(src: &RgbaImage, downsample: u32, radius: u32, iterations: u32) -> RgbaImage {
    let downsample = downsample.max(1);
    let target_w = src.width() / downsample;
    let target_h = src.height() / downsample;

    // Iteratively halve until further halving would undershoot the target.
    // The first halve reads directly from `src`, avoiding a full-res clone.
    let mut img: Option<RgbaImage> = None;
    loop {
        let current = img.as_ref().unwrap_or(src);
        if current.width() <= 1
            || current.height() <= 1
            || current.width() / 2 < target_w
            || current.height() / 2 < target_h
        {
            break;
        }
        img = Some(halve(current));
    }

    if radius == 0 || iterations == 0 {
        return img.unwrap_or_else(|| src.clone());
    }

    let img = img.unwrap_or_else(|| src.clone());
    let (w, h) = (img.width(), img.height());
    let mut a: Vec<u8> = img.into_raw(); // Rgba rows packed, length = w * h * 4
    let mut b: Vec<u8> = vec![0; a.len()];

    for _ in 0..iterations {
        // Horizontal pass: a → b
        box_blur_h(&a, &mut b, w as usize, h as usize, radius as usize);
        // Vertical pass: b → a
        box_blur_v(&b, &mut a, w as usize, h as usize, radius as usize);
    }

    RgbaImage::from_raw(w, h, a).expect("buffer size matches dims")
}

/// Average a 2×2 neighborhood to produce a half-resolution image. Edge handling
/// clamps to the input bounds.
fn halve(src: &RgbaImage) -> RgbaImage {
    let (sw, sh) = (src.width(), src.height());
    let (dw, dh) = ((sw / 2).max(1), (sh / 2).max(1));
    let src = src.as_raw();
    let mut out = vec![0u8; (dw as usize) * (dh as usize) * 4];

    let src_stride = (sw as usize) * 4;
    let dst_stride = (dw as usize) * 4;

    for dy in 0..dh as usize {
        let sy0 = dy * 2;
        let sy1 = (sy0 + 1).min(sh as usize - 1);
        let row0 = sy0 * src_stride;
        let row1 = sy1 * src_stride;
        let out_row = dy * dst_stride;
        for dx in 0..dw as usize {
            let sx0 = dx * 2;
            let sx1 = (sx0 + 1).min(sw as usize - 1);
            let p00 = row0 + sx0 * 4;
            let p01 = row0 + sx1 * 4;
            let p10 = row1 + sx0 * 4;
            let p11 = row1 + sx1 * 4;
            for c in 0..4 {
                let sum = src[p00 + c] as u16
                    + src[p01 + c] as u16
                    + src[p10 + c] as u16
                    + src[p11 + c] as u16;
                out[out_row + dx * 4 + c] = (sum / 4) as u8;
            }
        }
    }

    RgbaImage::from_raw(dw, dh, out).expect("halve: buffer matches dims")
}

/// Horizontal box blur with radius `r`, using a running-sum so each row is O(w).
/// Rgba layout; edges clamp.
fn box_blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let diameter = 2 * r + 1;
    let stride = w * 4;

    for y in 0..h {
        let row = y * stride;
        let mut sum = [0u32; 4];

        // Prime the sum with the first `diameter` pixels (clamped at left edge).
        for i in 0..diameter {
            let x = i.saturating_sub(r).min(w - 1);
            let p = row + x * 4;
            for c in 0..4 {
                sum[c] += src[p + c] as u32;
            }
        }

        let inv = diameter as u32;
        for x in 0..w {
            let out_p = row + x * 4;
            for c in 0..4 {
                dst[out_p + c] = (sum[c] / inv) as u8;
            }
            // Slide the window: remove leftmost, add rightmost (both clamped).
            let x_out = x.saturating_sub(r); // leaving the window
            let x_in = (x + r + 1).min(w - 1); // entering the window
            let p_out = row + x_out * 4;
            let p_in = row + x_in * 4;
            for c in 0..4 {
                sum[c] = sum[c] + src[p_in + c] as u32 - src[p_out + c] as u32;
            }
        }
    }
}

/// Vertical box blur with radius `r`, running-sum over columns.
fn box_blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || h == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let diameter = 2 * r + 1;
    let stride = w * 4;

    for x in 0..w {
        let col = x * 4;
        let mut sum = [0u32; 4];

        for i in 0..diameter {
            let y = i.saturating_sub(r).min(h - 1);
            let p = y * stride + col;
            for c in 0..4 {
                sum[c] += src[p + c] as u32;
            }
        }

        let inv = diameter as u32;
        for y in 0..h {
            let out_p = y * stride + col;
            for c in 0..4 {
                dst[out_p + c] = (sum[c] / inv) as u8;
            }
            let y_out = y.saturating_sub(r);
            let y_in = (y + r + 1).min(h - 1);
            let p_out = y_out * stride + col;
            let p_in = y_in * stride + col;
            for c in 0..4 {
                sum[c] = sum[c] + src[p_in + c] as u32 - src[p_out + c] as u32;
            }
        }
    }
}
