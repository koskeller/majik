//! Small image helpers and deterministic content generation, shared by tests and the library
//! seeding tool (`majik_generation::seed`).

use image::{Rgb, RgbImage};

/// Solid-colour PNG (used by tests and the mock provider fallback).
pub fn solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(width.max(1), height.max(1), image::Rgb(rgb));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).expect("in-memory PNG encode cannot fail");
    out.into_inner()
}

/// A picture that looks (and compresses) roughly like a generated one: a four-corner gradient,
/// a few soft blobs and a little grain, all derived from `seed`. Solid colours compress to a few
/// kilobytes, which would make a seeded library unrealistically cheap to read and decode; this is
/// a few hundred kilobytes at 1024 px, in the range of what a provider actually returns.
pub fn gradient_png(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let img = gradient_image(width, height, seed);
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).expect("in-memory PNG encode cannot fail");
    out.into_inner()
}

/// The picture behind [`gradient_png`].
pub fn gradient_image(width: u32, height: u32, seed: u64) -> RgbImage {
    let width = width.max(1);
    let height = height.max(1);
    let mut rng = Rng::new(seed);
    let corners: [[f32; 3]; 4] = [rng.color(), rng.color(), rng.color(), rng.color()];
    let blobs: Vec<Blob> = (0..rng.range(3, 7))
        .map(|_| Blob {
            x: rng.unit() * width as f32,
            y: rng.unit() * height as f32,
            radius: (0.15 + rng.unit() * 0.35) * width.min(height) as f32,
            color: rng.color(),
            strength: 0.3 + rng.unit() * 0.5,
        })
        .collect();

    let mut img = RgbImage::new(width, height);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let u = x as f32 / width as f32;
        let v = y as f32 / height as f32;
        let mut rgb = [0f32; 3];
        for (channel, value) in rgb.iter_mut().enumerate() {
            let top = corners[0][channel] * (1. - u) + corners[1][channel] * u;
            let bottom = corners[2][channel] * (1. - u) + corners[3][channel] * u;
            *value = top * (1. - v) + bottom * v;
        }
        for blob in &blobs {
            let dx = x as f32 - blob.x;
            let dy = y as f32 - blob.y;
            let distance = (dx * dx + dy * dy) / (blob.radius * blob.radius);
            if distance >= 1. {
                continue;
            }
            let falloff = (1. - distance) * (1. - distance) * blob.strength;
            for (channel, value) in rgb.iter_mut().enumerate() {
                *value += (blob.color[channel] - *value) * falloff;
            }
        }
        // Grain: enough to stop the encoder collapsing the whole picture into a few
        // kilobytes, little enough that the image still looks like a smooth render.
        let grain = rng.next_u64();
        let mut out = [0u8; 3];
        for (channel, value) in rgb.iter().enumerate() {
            let noise = ((grain >> (channel * 8)) & 0x0F) as f32 - 7.5;
            out[channel] = (value + noise).clamp(0., 255.) as u8;
        }
        *pixel = Rgb(out);
    }
    img
}

struct Blob {
    x: f32,
    y: f32,
    radius: f32,
    color: [f32; 3],
    strength: f32,
}

/// Deterministic SplitMix64. Seeded content has to be byte-identical across runs and platforms, so
/// the generators carry their own generator rather than pulling in `rand`.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `0.0..1.0`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// `0..n`, or 0 when `n` is 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    /// `low..=high`.
    pub fn range(&mut self, low: u32, high: u32) -> u32 {
        if high <= low {
            return low;
        }
        low + (self.next_u64() % u64::from(high - low + 1)) as u32
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f32) -> bool {
        self.unit() < p
    }

    /// One of `items`, or `None` when it is empty.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            items.get(self.below(items.len()))
        }
    }

    fn color(&mut self) -> [f32; 3] {
        [self.range(20, 235) as f32, self.range(20, 235) as f32, self.range(20, 235) as f32]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_png_is_deterministic_and_decodes_at_its_size() {
        let first = gradient_png(64, 48, 7);
        assert_eq!(first, gradient_png(64, 48, 7), "same seed, same bytes");
        assert_ne!(first, gradient_png(64, 48, 8), "a different seed looks different");
        let decoded = image::load_from_memory(&first).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 48));
    }

    #[test]
    fn gradient_png_carries_more_detail_than_a_solid_fill() {
        // The point of the generator: a seeded library reads and decodes like a real one.
        let solid = solid_png(256, 256, [10, 20, 30]);
        let gradient = gradient_png(256, 256, 1);
        assert!(gradient.len() > solid.len() * 20, "{} vs {}", gradient.len(), solid.len());
    }

    #[test]
    fn rng_stays_inside_its_bounds() {
        let mut rng = Rng::new(3);
        for _ in 0..1000 {
            let value = rng.range(5, 9);
            assert!((5..=9).contains(&value), "{value}");
            assert!(rng.below(4) < 4);
            assert!((0. ..1.).contains(&rng.unit()));
        }
        assert_eq!(rng.pick::<u8>(&[]), None);
    }
}
