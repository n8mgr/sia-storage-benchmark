//! Per-file results and the end-of-run report.

use std::time::Duration;

use crate::memory::Usage;
use crate::units::{self, ByteSize, bits_per_second};

/// One file's contribution to a run.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub index: u32,
    pub bytes: u64,
    pub encoded: u64,
    pub elapsed: Duration,
    pub ttfb: Option<Duration>,
}

/// A run aggregated across its files. Speeds are in bits per second.
#[derive(Clone, Debug)]
pub struct Summary {
    pub attempted: usize,
    pub succeeded: usize,
    pub concurrency: usize,
    pub bytes: u64,
    pub encoded: u64,
    pub wall: Duration,
    pub aggregate_bps: f64,
    pub mean_bps: f64,
    pub fastest: Option<Sample>,
    pub slowest: Option<Sample>,
    pub mean_ttfb: Option<Duration>,
    pub memory: Usage,
}

impl Sample {
    pub fn bps(&self) -> f64 {
        bits_per_second(self.bytes, self.elapsed)
    }
}

impl Summary {
    pub fn failed(&self) -> usize {
        self.attempted - self.succeeded
    }

    pub fn print(&self, label: &str) {
        let row = |name: &str, value: String| println!("  {name:<14}{value}");
        println!("\n{label}");
        row("Files", format!("{} of {}", self.succeeded, self.attempted));
        row("Concurrency", self.concurrency.to_string());
        row(
            "Transferred",
            format!(
                "{} ({} encoded)",
                ByteSize(self.bytes),
                ByteSize(self.encoded)
            ),
        );
        row("Wall clock", units::duration(self.wall));
        if self.memory.peak > 0 {
            row(
                "Peak RSS",
                format!(
                    "{} of {}",
                    ByteSize(self.memory.peak),
                    ByteSize(self.memory.system_total)
                ),
            );
        }
        row("Aggregate", units::bitrate(self.aggregate_bps));

        let extreme = |s: Sample| {
            format!(
                "{} (file {}, {})",
                units::bitrate(s.bps()),
                s.index,
                units::duration(s.elapsed)
            )
        };
        if let Some(s) = self.fastest {
            row("Fastest", extreme(s));
        }
        if let Some(s) = self.slowest {
            row("Slowest", extreme(s));
        }
        if self.succeeded > 0 {
            row("Average", units::bitrate(self.mean_bps));
        }
        if let Some(ttfb) = self.mean_ttfb {
            row("Mean TTFB", units::duration(ttfb));
        }
    }
}

/// Aggregates the successful files of a run. `attempted` counts every file the
/// run tried, so failures show up even though they contribute no samples.
pub fn summarize(
    samples: &[Sample],
    attempted: usize,
    concurrency: usize,
    wall: Duration,
    memory: Usage,
) -> Summary {
    let bytes = samples.iter().map(|s| s.bytes).sum();
    let speeds: Vec<f64> = samples.iter().map(Sample::bps).collect();
    let ttfbs: Vec<Duration> = samples.iter().filter_map(|s| s.ttfb).collect();
    Summary {
        attempted,
        succeeded: samples.len(),
        concurrency,
        bytes,
        encoded: samples.iter().map(|s| s.encoded).sum(),
        wall,
        aggregate_bps: bits_per_second(bytes, wall),
        mean_bps: speeds.iter().sum::<f64>() / speeds.len().max(1) as f64,
        fastest: samples
            .iter()
            .copied()
            .max_by(|a, b| a.bps().total_cmp(&b.bps())),
        slowest: samples
            .iter()
            .copied()
            .min_by(|a, b| a.bps().total_cmp(&b.bps())),
        mean_ttfb: (!ttfbs.is_empty()).then(|| ttfbs.iter().sum::<Duration>() / ttfbs.len() as u32),
        memory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(index: u32, secs: u64) -> Sample {
        Sample {
            index,
            bytes: 1_000_000,
            encoded: 3_000_000,
            elapsed: Duration::from_secs(secs),
            ttfb: Some(Duration::from_millis(100 * u64::from(index))),
        }
    }

    #[test]
    fn extremes_and_means() {
        let samples = [sample(1, 1), sample(2, 2), sample(3, 4)];
        let s = summarize(&samples, 4, 2, Duration::from_secs(5), Usage::default());

        assert_eq!((s.succeeded, s.attempted, s.failed()), (3, 4, 1));
        assert_eq!((s.bytes, s.encoded), (3_000_000, 9_000_000));
        assert_eq!(s.fastest.unwrap().index, 1, "1s is fastest");
        assert_eq!(s.slowest.unwrap().index, 3, "4s is slowest");
        assert_eq!(s.fastest.unwrap().bps(), 8e6);
        assert_eq!(s.slowest.unwrap().bps(), 2e6);
        assert_eq!(s.mean_bps, (8e6 + 4e6 + 2e6) / 3.0);
        // 3 MB over 5s of wall clock, not the 7s summed across files.
        assert_eq!(s.aggregate_bps, 24e6 / 5.0);
        assert_eq!(s.mean_ttfb, Some(Duration::from_millis(200)));
    }

    #[test]
    fn empty_run() {
        let s = summarize(&[], 2, 1, Duration::from_secs(1), Usage::default());
        assert_eq!((s.succeeded, s.failed()), (0, 2));
        assert_eq!(s.mean_bps, 0.0);
        assert!(s.fastest.is_none() && s.slowest.is_none() && s.mean_ttfb.is_none());
    }
}
