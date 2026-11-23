//! Reads a frame ring the way the UI does, for checks by hand:
//!
//! ```text
//! ring_probe <ring file> <seconds> [<png file>]
//! ```
//!
//! Prints the ring's size and generation, how many new frames it showed in `seconds` and at what
//! rate, how it ended if it did, and saves the last frame read as a PNG.

use std::path::Path;
use std::time::{Duration, Instant};

use rustet_framering::{Read, Reader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(ring), Some(secs)) = (args.first(), args.get(1)) else {
        return Err("usage: ring_probe <ring file> <seconds> [<png file>]".into());
    };
    let secs: f64 = secs.parse()?;
    let mut reader = Reader::open(Path::new(ring))?;
    let f = reader.format();
    let mut buf = Vec::new();
    let (mut frames, mut first, mut last, mut end) = (0u64, None, None, None);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs_f64(secs) {
        match reader.read(&mut buf) {
            Read::Frame(fr) => {
                frames += 1;
                first.get_or_insert(fr.presented_at_us);
                last = Some(fr.presented_at_us);
            }
            Read::Unchanged => std::thread::sleep(Duration::from_millis(4)),
            Read::Ended(e) => {
                end = Some(e);
                break;
            }
        }
    }
    let fps = match (first, last) {
        (Some(a), Some(b)) if b > a => (frames - 1) as f64 * 1e6 / (b - a) as f64,
        _ => 0.0,
    };
    println!(
        "ring generation {} size {}x{} frames {frames} fps {fps:.1} ended {end:?}",
        reader.generation(),
        f.width,
        f.height
    );
    if let Some(png_path) = args.get(2).filter(|_| !buf.is_empty()) {
        // BGRA → RGBA.
        let rgba: Vec<u8> = buf
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0], p[3]])
            .collect();
        let mut enc = png::Encoder::new(std::fs::File::create(png_path)?, f.width, f.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(&rgba)?;
        println!("saved {png_path}");
    }
    Ok(())
}
