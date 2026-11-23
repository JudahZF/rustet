//! Volume ramps for crossfades.

use std::time::Duration;

/// A straight-line volume ramp in libvlc's volume scale (0–200, 100 = 100%), for crossfades:
/// the caller samples it on its own timer and applies the level with
/// [`Deck::set_volume`](crate::Deck::set_volume), one ramp per deck.
///
/// libvlc's PulseAudio output and its software volume both map the level to a cubic gain, so a
/// straight line here is a perceptually even fade, not a linear-amplitude one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ramp {
    pub from: u32,
    pub to: u32,
    pub duration: Duration,
}

impl Ramp {
    /// The level `elapsed` after the ramp started: `from` at 0, `to` from `duration` on.
    pub fn level_at(&self, elapsed: Duration) -> u32 {
        if elapsed >= self.duration {
            return self.to;
        }
        let t = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        let level = f64::from(self.from) + (f64::from(self.to) - f64::from(self.from)) * t;
        level.round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ends must be exact: a fade-out that stops at 1 leaks sound, a fade-in that stops at 99
    // leaves the next track quieter than the user's volume.
    #[test]
    fn ramp_hits_its_ends() {
        let out = Ramp {
            from: 80,
            to: 0,
            duration: Duration::from_secs(4),
        };
        assert_eq!(out.level_at(Duration::ZERO), 80);
        assert_eq!(out.level_at(Duration::from_secs(1)), 60);
        assert_eq!(out.level_at(Duration::from_secs(4)), 0);
        assert_eq!(out.level_at(Duration::from_secs(9)), 0);
        let cut = Ramp {
            from: 0,
            to: 100,
            duration: Duration::ZERO,
        };
        assert_eq!(cut.level_at(Duration::ZERO), 100);
    }
}
