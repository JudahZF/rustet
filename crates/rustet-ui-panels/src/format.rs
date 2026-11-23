//! Text for sizes, speeds and times.

use std::time::Duration;

/// "820 KB", "3.4 MB", "1.2 GB", in units of 1024 as the engine counts its limits (a
/// 500 MB limit is 500 × 1024 × 1024 bytes).
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1024.;
    let mut unit = 0;
    while v >= 1024. && unit < UNITS.len() - 1 {
        v /= 1024.;
        unit += 1;
    }
    if v < 10. {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// Megabytes (of 1024 × 1024 bytes) as a limit: "500 MB", "2 GB", "1.5 GB".
pub fn megabytes(mb: u64) -> String {
    if mb >= 1024 {
        let gb = mb as f64 / 1024.;
        if gb.fract() == 0. {
            format!("{gb:.0} GB")
        } else {
            format!("{gb:.1} GB")
        }
    } else {
        format!("{mb} MB")
    }
}

/// "3 h 20 min", "12 min", "less than a minute".
pub fn span(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "less than a minute".to_owned(),
        (0, m) => format!("{m} min"),
        (h, _) if h >= 48 => format!("{} days", h / 24),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// "just now", "5 minutes ago", "3 hours ago", "2 days ago".
pub fn ago(d: Duration) -> String {
    let s = d.as_secs();
    let (n, unit) = match s {
        0..60 => return "just now".to_owned(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        _ => (s / 86_400, "day"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// "1 song", "64 songs".
pub fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "1st", "2nd", "3rd", "11th".
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_times_read_naturally() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(3_565_158), "3.4 MB");
        assert_eq!(bytes(500 << 20), "500 MB");
        assert_eq!(bytes(2 << 30), "2.0 GB");
        assert_eq!(megabytes(500), "500 MB");
        assert_eq!(megabytes(2048), "2 GB");
        assert_eq!(megabytes(1536), "1.5 GB");
        assert_eq!(span(Duration::from_secs(3 * 3600 + 20 * 60)), "3 h 20 min");
        assert_eq!(span(Duration::from_secs(30)), "less than a minute");
        assert_eq!(span(Duration::from_secs(5 * 86_400)), "5 days");
        assert_eq!(ago(Duration::from_secs(61)), "1 minute ago");
        assert_eq!(ago(Duration::from_secs(7200)), "2 hours ago");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(23), "23rd");
    }
}
