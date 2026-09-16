//! Byte sizes, speeds, and durations.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

/// A size in bytes. KiB, MiB, GiB, and TiB are binary; KB, MB, GB, and TB are
/// decimal; bare K, M, G, and T are binary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(pub u64);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ParseSizeError(String);

impl ByteSize {
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
        let mut v = self.0 as f64;
        let mut i = 0;
        while v >= 1024.0 && i < UNITS.len() - 1 {
            v /= 1024.0;
            i += 1;
        }
        match i {
            0 => write!(f, "{} B", self.0),
            _ => write!(f, "{} {}", trim(v), UNITS[i]),
        }
    }
}

impl FromStr for ByteSize {
    type Err = ParseSizeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let split = s
            .rfind(|c: char| c.is_ascii_digit() || c == '.')
            .map_or(0, |i| i + 1);
        let (num, unit) = (s[..split].trim(), s[split..].trim().to_ascii_lowercase());
        let value: f64 = num
            .parse()
            .map_err(|_| ParseSizeError(format!("invalid size {s:?}")))?;
        let scale: f64 = match unit.as_str() {
            "" | "b" => 1.0,
            "k" | "kib" => 1024.0,
            "kb" => 1e3,
            "m" | "mib" => 1024.0f64.powi(2),
            "mb" => 1e6,
            "g" | "gib" => 1024.0f64.powi(3),
            "gb" => 1e9,
            "t" | "tib" => 1024.0f64.powi(4),
            "tb" => 1e12,
            _ => return Err(ParseSizeError(format!("unknown unit {unit:?} in {s:?}"))),
        };
        let bytes = value * scale;
        if !(0.0..=i64::MAX as f64).contains(&bytes) {
            return Err(ParseSizeError(format!("size {s:?} out of range")));
        }
        Ok(ByteSize(bytes as u64))
    }
}

/// Two decimals with trailing zeros trimmed, so whole numbers stay whole.
fn trim(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A speed with an SI prefix, e.g. "94.3 Mbps".
pub fn bitrate(bps: f64) -> String {
    const UNITS: [&str; 5] = ["bps", "kbps", "Mbps", "Gbps", "Tbps"];
    let mut v = bps;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    format!("{} {}", trim(v), UNITS[i])
}

pub fn duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 3600.0 {
        format!("{:.2}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.1}m", secs / 60.0)
    } else if secs >= 1.0 {
        format!("{secs:.2}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

pub fn bits_per_second(bytes: u64, elapsed: Duration) -> f64 {
    match elapsed.as_secs_f64() {
        secs if secs > 0.0 => bytes as f64 * 8.0 / secs,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        for (input, want) in [
            ("1024", 1024),
            ("5MiB", 5 << 20),
            ("5MB", 5_000_000),
            ("10GB", 10_000_000_000),
            ("1.5GiB", 3 << 29),
            ("64m", 64 << 20),
            (" 2 KiB", 2048),
        ] {
            assert_eq!(
                input.parse::<ByteSize>().unwrap(),
                ByteSize(want),
                "{input}"
            );
        }
        for input in ["", "abc", "5XB", "-1"] {
            assert!(input.parse::<ByteSize>().is_err(), "{input:?}");
        }
    }

    #[test]
    fn render() {
        assert_eq!(ByteSize(512).to_string(), "512 B");
        assert_eq!(ByteSize(100 << 20).to_string(), "100 MiB");
        assert_eq!(ByteSize(3 << 29).to_string(), "1.5 GiB");
        assert_eq!(ByteSize(5_000_000_000_000).to_string(), "4.55 TiB");

        assert_eq!(bitrate(0.0), "0 bps");
        assert_eq!(bitrate(94_300_000.0), "94.3 Mbps");
        assert_eq!(bitrate(1.5e9), "1.5 Gbps");

        assert_eq!(duration(Duration::from_millis(250)), "250ms");
        assert_eq!(duration(Duration::from_secs(90)), "1.5m");
        assert_eq!(duration(Duration::from_secs(9000)), "2.50h");

        assert_eq!(bits_per_second(1_000_000, Duration::from_secs(1)), 8e6);
        assert_eq!(bits_per_second(1_000_000, Duration::ZERO), 0.0);
    }
}
