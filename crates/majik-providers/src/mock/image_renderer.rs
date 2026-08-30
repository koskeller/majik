//! Generates deterministic solid-color PNGs for the mock provider.
//!
//! Color is a function of `SHA256(provider|width×height|prompt|model)`, so the same inputs always
//! produce byte-identical output. Different prompts (or models, sizes, providers) produce visually
//! distinct colors.

use std::io::Cursor;

use image::{ImageFormat, Rgb, RgbImage};
use sha2::{Digest, Sha256};

use crate::models::{AspectRatio, ImageModel, ImageResolution};
use crate::ProviderId;

/// Solid RGB colour.
pub type Color = [u8; 3];

pub fn render(provider: &ProviderId, model: &ImageModel, clean_prompt: &str, aspect_ratio: Option<AspectRatio>, resolution: Option<ImageResolution>) -> Vec<u8> {
    let (width, height) = pixel_size(aspect_ratio, resolution);
    let color = color(provider, model.id, clean_prompt, width, height);
    render_png(width, height, color).or_else(|| render_png(1, 1, color)).unwrap_or_default()
}

/// Returns the output dimensions for an image aspect ratio + resolution pair. Longest edge is set
/// by the resolution bucket; the shorter edge is computed from the aspect ratio and rounded down to
/// even pixels.
pub fn pixel_size(aspect_ratio: Option<AspectRatio>, resolution: Option<ImageResolution>) -> (u32, u32) {
    let long_edge = match resolution.unwrap_or(ImageResolution::Hd) {
        ImageResolution::Sd => 512,
        ImageResolution::Hd => 1024,
        ImageResolution::Fhd => 2048,
        ImageResolution::Uhd => 3840,
    };
    let (num, denom) = parse_ratio(aspect_ratio.unwrap_or(AspectRatio::Square).raw(), (1, 1));
    fit_longest_edge(long_edge, num, denom)
}

/// `SHA256("provider|WxH|prompt|model")` → first three bytes as RGB.
pub fn color(provider: &ProviderId, model: &str, prompt: &str, width: u32, height: u32) -> Color {
    let seed = format!("{}|{}x{}|{}|{}", provider.as_str(), width, height, prompt, model);
    let hash = Sha256::digest(seed.as_bytes());
    [hash[0], hash[1], hash[2]]
}

fn render_png(width: u32, height: u32, color: Color) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let img = RgbImage::from_pixel(width, height, Rgb(color));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

// ----- shared ratio helpers -------------------------------------------------------------------

/// Parses a `"W:H"` string into integer components, falling back to `fallback` on any parse failure.
pub fn parse_ratio(raw: &str, fallback: (u32, u32)) -> (u32, u32) {
    let mut parts = raw.split(':');
    let (Some(n), Some(d), None) = (parts.next(), parts.next(), parts.next()) else {
        return fallback;
    };
    match (n.parse::<u32>(), d.parse::<u32>()) {
        (Ok(num), Ok(denom)) => (num, denom),
        _ => fallback,
    }
}

/// Given a target longest-edge pixel count and an aspect ratio (`num:denom`), returns
/// `(width, height)` with the shorter edge rounded down to an even number of pixels.
pub fn fit_longest_edge(long_edge: u32, num: u32, denom: u32) -> (u32, u32) {
    let short = |a: u32, b: u32| -> u32 {
        let v = (long_edge as f64 * a as f64 / b as f64) as u32 & !1;
        v.max(2)
    };
    if num >= denom {
        (long_edge, short(denom, num))
    } else {
        (short(num, denom), long_edge)
    }
}
