//! Text for times and stream formats, shared by every view so they read the same.

use std::time::Duration;

/// `m:ss`, or `h:mm:ss` from an hour (track and position times; draw with tabular figures).
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// A total length in words ("48 minutes", "1 hour 12 minutes") for headers.
pub fn length(d: Duration) -> String {
    let minutes = (d.as_secs() + 30) / 60;
    let (h, m) = (minutes / 60, minutes % 60);
    let plural = |n: u64, word: &str| {
        if n == 1 {
            format!("1 {word}")
        } else {
            format!("{n} {word}s")
        }
    };
    match (h, m) {
        (0, m) => plural(m, "minute"),
        (h, 0) => plural(h, "hour"),
        (h, m) => format!("{} {}", plural(h, "hour"), plural(m, "minute")),
    }
}

/// A stream's bitrate as people read it: `Quality.bitrate` is in bits per second (256000),
/// shown in kilobits ("256 kbps").
pub fn kbps(bits_per_second: u32) -> String {
    format!("{} kbps", (bits_per_second + 500) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrates_read_in_kilobits() {
        assert_eq!(kbps(256_000), "256 kbps");
        assert_eq!(kbps(255_600), "256 kbps");
        assert_eq!(kbps(64_000), "64 kbps");
    }

    #[test]
    fn times_and_lengths() {
        assert_eq!(duration(Duration::from_secs(5)), "0:05");
        assert_eq!(duration(Duration::from_secs(245)), "4:05");
        assert_eq!(duration(Duration::from_secs(3725)), "1:02:05");
        assert_eq!(length(Duration::from_secs(48 * 60 + 10)), "48 minutes");
        assert_eq!(length(Duration::from_secs(60)), "1 minute");
        assert_eq!(length(Duration::from_secs(72 * 60)), "1 hour 12 minutes");
        assert_eq!(length(Duration::from_secs(120 * 60)), "2 hours");
    }
}
