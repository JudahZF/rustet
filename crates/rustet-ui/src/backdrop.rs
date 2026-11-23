//! The blurred-artwork page background.
//!
//! The artwork is shrunk to 48 px, Gaussian-blurred there (cheap: a blur at 48 px looks like
//! a huge-radius blur at full size), darkened, and scaled to 384 px with a smooth filter so the
//! GPU stretch to window size shows no blocks. Results are kept in memory and on disk by
//! artwork URL. A track change cross-fades the new backdrop over the old one.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, Context, ElementId, ObjectFit, RenderImage, Task, Window, div, img,
    prelude::*,
};
use image::{DynamicImage, imageops::FilterType};

use crate::artwork::{self, DiskCache};
use crate::runtime;

const BLUR_EDGE: u32 = 48;
const OUTPUT_EDGE: u32 = 384;
const FADE: Duration = Duration::from_millis(700);

/// Blurs encoded artwork into a backdrop image (RGBA). Pure CPU work: run it off the UI thread.
pub fn blur(encoded: &[u8]) -> Result<image::RgbaImage, image::ImageError> {
    let small =
        image::load_from_memory(encoded)?.resize_exact(BLUR_EDGE, BLUR_EDGE, FilterType::Triangle);
    let blurred = image::imageops::blur(&small.into_rgba8(), 5.);
    let mut out = DynamicImage::ImageRgba8(blurred)
        .resize_exact(OUTPUT_EDGE, OUTPUT_EDGE, FilterType::CatmullRom)
        .into_rgba8();
    // Darken, so text on top keeps its contrast whatever the artwork.
    for px in out.pixels_mut() {
        for c in &mut px.0[..3] {
            *c = (f32::from(*c) * 0.62) as u8;
        }
    }
    Ok(out)
}

struct Layer {
    id: usize,
    image: Arc<RenderImage>,
}

/// The backdrop view: shows the latest backdrop, fading in over the previous one.
pub struct Backdrop {
    http: reqwest::Client,
    disk: Option<Arc<DiskCache>>,
    /// Recent backdrops by URL, oldest first, at most [`MEMORY_ENTRIES`].
    memory: Vec<(String, Arc<RenderImage>)>,
    /// Backdrops dropped from memory and not on screen: their textures are freed at the next
    /// render (the only place with the window).
    evicted: Vec<Arc<RenderImage>>,
    current: Option<Layer>,
    previous: Option<Layer>,
    next_id: usize,
    loading: Option<Task<()>>,
}

impl Backdrop {
    pub fn new(http: reqwest::Client, disk: Option<Arc<DiskCache>>) -> Self {
        Self {
            http,
            disk,
            memory: Vec::new(),
            evicted: Vec::new(),
            current: None,
            previous: None,
            next_id: 0,
            loading: None,
        }
    }

    /// Switches to the backdrop for `url` (a sized artwork URL). A newer call cancels a
    /// pending one.
    pub fn show(&mut self, url: String, cx: &mut Context<Self>) {
        if let Some(image) = self
            .memory
            .iter()
            .find(|(u, _)| *u == url)
            .map(|(_, i)| i.clone())
        {
            self.push(image);
            cx.notify();
            return;
        }
        let (http, disk, key) = (self.http.clone(), self.disk.clone(), url.clone());
        let job = runtime::spawn(cx, async move {
            let cache_key = format!("backdrop:{key}");
            let cached = {
                let (disk, k) = (disk.clone(), cache_key.clone());
                tokio::task::spawn_blocking(move || disk.and_then(|d| d.get(&k)))
                    .await
                    .ok()
                    .flatten()
            };
            let rgba = match cached.and_then(|png| image::load_from_memory(&png).ok()) {
                Some(img) => img.into_rgba8(),
                None => {
                    let bytes = http
                        .get(&key)
                        .send()
                        .await
                        .ok()?
                        .error_for_status()
                        .ok()?
                        .bytes()
                        .await
                        .ok()?;
                    tokio::task::spawn_blocking(move || {
                        let rgba = blur(&bytes).ok()?;
                        if let Some(disk) = disk {
                            let mut png = Vec::new();
                            if DynamicImage::ImageRgba8(rgba.clone())
                                .write_to(
                                    &mut std::io::Cursor::new(&mut png),
                                    image::ImageFormat::Png,
                                )
                                .is_ok()
                            {
                                let _ = disk.put(&cache_key, &png);
                            }
                        }
                        Some(rgba)
                    })
                    .await
                    .ok()??
                }
            };
            Some(Arc::new(artwork::to_render_image(rgba)))
        });
        self.loading = Some(cx.spawn(async move |this, cx| {
            if let Ok(Some(image)) = job.await {
                this.update(cx, |this, cx| {
                    this.remember(url, image.clone());
                    this.push(image);
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn remember(&mut self, url: String, image: Arc<RenderImage>) {
        self.memory.push((url, image));
        if self.memory.len() > MEMORY_ENTRIES {
            let (_, old) = self.memory.remove(0);
            let on_screen = [&self.current, &self.previous]
                .into_iter()
                .flatten()
                .any(|l| Arc::ptr_eq(&l.image, &old));
            if !on_screen {
                self.evicted.push(old);
            }
        }
    }

    fn push(&mut self, image: Arc<RenderImage>) {
        if self
            .current
            .as_ref()
            .is_some_and(|c| Arc::ptr_eq(&c.image, &image))
        {
            return;
        }
        // The layer two changes back is no longer drawn; free its texture unless remembered.
        if let Some(old) = self.previous.take()
            && !self.memory.iter().any(|(_, m)| Arc::ptr_eq(m, &old.image))
        {
            self.evicted.push(old.image);
        }
        self.previous = self.current.take();
        self.next_id += 1;
        self.current = Some(Layer {
            id: self.next_id,
            image,
        });
    }
}

/// Backdrops kept in memory (each 384×384 BGRA, 590 kB).
const MEMORY_ENTRIES: usize = 16;

impl Render for Backdrop {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        for image in self.evicted.drain(..) {
            window.drop_image(image).ok();
        }
        let layer = |l: &Layer| {
            img(l.image.clone())
                .absolute()
                .inset_0()
                .size_full()
                .object_fit(ObjectFit::Cover)
        };
        div()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .children(self.previous.as_ref().map(layer))
            .children(self.current.as_ref().map(|l| {
                layer(l).with_animation(
                    ElementId::NamedInteger("backdrop".into(), l.id as u64),
                    Animation::new(FADE).with_easing(gpui::ease_in_out),
                    |el, t| el.opacity(t),
                )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_produces_a_smooth_dark_square() {
        // A hard-edged half-white, half-black image.
        let mut src = image::RgbaImage::new(200, 100);
        for (x, _, px) in src.enumerate_pixels_mut() {
            *px = if x < 100 {
                image::Rgba([255, 255, 255, 255])
            } else {
                image::Rgba([0, 0, 0, 255])
            };
        }
        let mut png = Vec::new();
        DynamicImage::ImageRgba8(src)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let out = blur(&png).unwrap();
        assert_eq!(out.dimensions(), (OUTPUT_EDGE, OUTPUT_EDGE));
        // Darkened: even the white side is well below full brightness.
        assert!(out.get_pixel(10, 192)[0] < 170);
        // Smooth: the middle is a gradient, not a step.
        let mid = out.get_pixel(OUTPUT_EDGE / 2, 192)[0];
        assert!(mid > 30 && mid < 140, "{mid}");
    }
}
