//! Quality: the neutral marks ("Lossless", "Hi-Res Lossless", "Dolby Atmos", in text
//! secondary as Apple's badges are) and the quality strip, the format of the stream that is
//! actually playing, set as type ("ALAC 24-bit 96 kHz", "AAC 256 kbps"), digits in tabular
//! figures. When the format changes, the old text crossfades to the new one in 200 ms
//! (instant with Reduce animations).

use std::time::Instant;

use gpui::{Context, Role, SharedString, Window, div, prelude::*};
use rustet_player::model::{Codec, Quality, QualityTier, Spatial};

use crate::theme::{ActiveTheme, Motion, SEMIBOLD, Theme};

/// A quality mark after a title ("Lossless", "Dolby Atmos"): 12 / 600, text secondary.
pub fn quality_mark(theme: &Theme, label: &'static str) -> impl IntoElement {
    div()
        .flex_none()
        .text_size(theme.text.callout)
        .font_weight(SEMIBOLD)
        .text_color(theme.colors.text_secondary)
        .child(label)
}

/// The badge of a playing stream: "Hi-Res Lossless", "Lossless", "Dolby Atmos", or none for
/// AAC and video.
pub fn quality_badge(q: &Quality) -> Option<&'static str> {
    match q.tier {
        QualityTier::HiResLossless => Some("Hi-Res Lossless"),
        QualityTier::Lossless => Some("Lossless"),
        QualityTier::DolbyAtmos => Some("Dolby Atmos"),
        QualityTier::Aac | QualityTier::Spatial | QualityTier::Video => None,
    }
}

/// The strip's text: the format as type.
pub fn quality_text(q: &Quality) -> SharedString {
    let khz = |rate: u32| {
        let k = f64::from(rate) / 1000.;
        if (k - k.round()).abs() < 0.05 {
            format!("{k:.0} kHz")
        } else {
            format!("{k:.1} kHz")
        }
    };
    let text = match q.codec {
        Codec::Alac => match (q.bit_depth, q.sample_rate) {
            (Some(bits), Some(rate)) => format!("ALAC {bits}-bit {}", khz(rate)),
            (None, Some(rate)) => format!("ALAC {}", khz(rate)),
            _ => "ALAC".to_owned(),
        },
        Codec::Aac => match q.bitrate {
            Some(bps) => format!("AAC {}", crate::format::kbps(bps)),
            None => "AAC".to_owned(),
        },
        Codec::Atmos => "Dolby Atmos".to_owned(),
        Codec::H264 => match q.video_height {
            Some(h) => format!("Video {h}p"),
            None => "Video".to_owned(),
        },
    };
    let text = match (q.spatial, q.codec) {
        (Some(Spatial::Binaural | Spatial::BinauralLossless), Codec::Alac | Codec::Aac) => {
            format!("{text}, spatial")
        }
        _ => text,
    };
    text.into()
}

/// The strip. The shell's player owns one and calls [`QualityStrip::set`] on every player state.
pub struct QualityStrip {
    current: Option<Quality>,
    previous: Option<Quality>,
    changed_at: Instant,
}

impl QualityStrip {
    pub fn new() -> Self {
        Self {
            current: None,
            previous: None,
            changed_at: Instant::now(),
        }
    }

    /// The playing stream's quality; `None` when nothing plays.
    pub fn set(&mut self, quality: Option<Quality>, cx: &mut Context<Self>) {
        if quality == self.current {
            return;
        }
        self.previous = self.current.take();
        self.current = quality;
        self.changed_at = Instant::now();
        cx.notify();
    }
}

impl Default for QualityStrip {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for QualityStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fade = theme.motion(Motion::QualityChange);
        let t = if fade.is_zero() {
            1.
        } else {
            (self.changed_at.elapsed().as_secs_f32() / fade.as_secs_f32()).min(1.)
        };
        if t < 1. {
            window.request_animation_frame();
        }
        let line = |q: &Quality, opacity: f32| {
            div()
                .absolute()
                .left_0()
                .top_0()
                .whitespace_nowrap()
                .text_size(theme.text.callout)
                .font_features(theme.tabular())
                .text_color(theme.colors.text_secondary)
                .opacity(opacity)
                .child(quality_text(q))
        };
        let label: SharedString = self
            .current
            .as_ref()
            .map_or_else(|| "Nothing playing".into(), quality_text);
        div()
            .id("quality-strip")
            .role(Role::Status)
            .aria_label(label.clone())
            .relative()
            .h(theme.text.body_line)
            // An invisible copy of the text gives the strip its width.
            .child(
                div()
                    .invisible()
                    .whitespace_nowrap()
                    .text_size(theme.text.callout)
                    .font_features(theme.tabular())
                    .child(label),
            )
            .children(
                self.previous
                    .as_ref()
                    .filter(|_| t < 1.)
                    .map(|q| line(q, 1. - t)),
            )
            .children(self.current.as_ref().map(|q| line(q, t)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(codec: Codec, tier: QualityTier) -> Quality {
        Quality {
            codec,
            tier,
            bit_depth: None,
            sample_rate: None,
            bitrate: None,
            channels: Some(2),
            spatial: None,
            video_height: None,
            fell_back: false,
        }
    }

    #[test]
    fn formats_read_as_type() {
        let hires = Quality {
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            ..q(Codec::Alac, QualityTier::HiResLossless)
        };
        assert_eq!(quality_text(&hires), "ALAC 24-bit 96 kHz");
        assert_eq!(quality_badge(&hires), Some("Hi-Res Lossless"));
        let cd = Quality {
            bit_depth: Some(16),
            sample_rate: Some(44_100),
            ..q(Codec::Alac, QualityTier::Lossless)
        };
        assert_eq!(quality_text(&cd), "ALAC 16-bit 44.1 kHz");
        let aac = Quality {
            bitrate: Some(256_000),
            ..q(Codec::Aac, QualityTier::Aac)
        };
        assert_eq!(quality_text(&aac), "AAC 256 kbps");
        assert_eq!(quality_badge(&aac), None);
        assert_eq!(
            quality_text(&q(Codec::Atmos, QualityTier::DolbyAtmos)),
            "Dolby Atmos"
        );
    }
}
