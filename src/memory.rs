//! The process's peak resident set, sampled while a transfer runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::task::JoinHandle;

const INTERVAL: Duration = Duration::from_secs(1);

/// A zero `peak` means the process could not be read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub peak: u64,
    pub system_total: u64,
}

pub struct Sampler {
    peak: Arc<AtomicU64>,
    system_total: u64,
    task: JoinHandle<()>,
}

impl Sampler {
    /// Starts sampling, taking the first reading immediately.
    pub fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(0));
        let mut system = System::new();
        system.refresh_memory();

        let task = tokio::spawn({
            let peak = peak.clone();
            async move {
                let Ok(pid) = sysinfo::get_current_pid() else {
                    return;
                };
                let mut system = System::new();
                let mut ticker = tokio::time::interval(INTERVAL);
                let kind = ProcessRefreshKind::nothing().with_memory();
                loop {
                    ticker.tick().await;
                    system.refresh_processes_specifics(
                        ProcessesToUpdate::Some(&[pid]),
                        false,
                        kind,
                    );
                    if let Some(process) = system.process(pid) {
                        peak.fetch_max(process.memory(), Ordering::Relaxed);
                    }
                }
            }
        });
        Self {
            peak,
            system_total: system.total_memory(),
            task,
        }
    }

    pub fn stop(self) -> Usage {
        self.task.abort();
        Usage {
            peak: self.peak.load(Ordering::Relaxed),
            system_total: self.system_total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn samples_until_stopped() {
        let sampler = Sampler::start();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let usage = sampler.stop();

        // Also pins the unit: sysinfo reports bytes, not the KiB of its
        // pre-0.30 releases, which would land this test process under 1 MiB.
        assert!(
            (1 << 20..=usage.system_total).contains(&usage.peak),
            "peak {} is not a plausible byte count against {} of system memory",
            usage.peak,
            usage.system_total
        );
    }
}
