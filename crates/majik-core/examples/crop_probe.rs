//! Throwaway: a library with images whose top, middle and bottom (or left, middle, right) are
//! different colours, to see which part a square grid cell shows.
use majik_core::library::Library;
use majik_core::model::MediaType;

fn banded(width: u32, height: u32, vertical: bool) -> Vec<u8> {
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        let t = if vertical { y as f32 / height as f32 } else { x as f32 / width as f32 };
        image::Rgb(if t < 0.33 { [220, 40, 40] } else if t < 0.66 { [40, 200, 60] } else { [40, 60, 220] })
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn main() {
    let root = std::env::args().nth(1).expect("library root");
    let mut lib = Library::open(&root).unwrap();
    for (w, h, vertical) in [(600, 1400, true), (1400, 600, false), (600, 1400, true)] {
        let id = lib.add_generating(MediaType::Image, None, Some("Probe".into()), Some("Mock".into()), None);
        lib.complete_generation(&id, &banded(w, h, vertical), false).unwrap();
    }
    println!("seeded {root}");
}
