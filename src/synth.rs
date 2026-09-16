//! The synthetic benchmarks: `n` files of `k` bytes with `m` transfers in
//! flight.
//!
//! `upload` generates the files, uploads and pins each one, and records them in
//! a manifest. `download` replays that manifest, fetching every object back and
//! verifying every byte. Both report the fastest, slowest, and average per-file
//! speed alongside the aggregate speed of the run.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sia_storage::{DownloadOptions, Object, Sdk, UploadOptions};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};

use crate::data::Data;
use crate::manifest::{Manifest, Record};
use crate::memory::Sampler;
use crate::stats::{Sample, summarize};
use crate::units::{self, ByteSize, bits_per_second};

pub struct UploadArgs {
    pub count: u32,
    pub size: u64,
    pub concurrency: usize,
    pub manifest: PathBuf,
    pub seed: u64,
    pub options: UploadOptions,
}

pub struct DownloadArgs {
    pub manifest: PathBuf,
    pub concurrency: Option<usize>,
    pub options: DownloadOptions,
}

/// The result of one file, tagged with its index so failures can be reported
/// even though they produce nothing.
struct Outcome<T> {
    index: u32,
    result: Result<T>,
}

/// A file whose bytes are on hosts but whose slabs are not yet pinned.
struct Uploaded {
    index: u32,
    object: Object,
    upload: Duration,
}

/// Starts `concurrency` workers that pull file indexes below `count` and run
/// `task` on each, and returns the handles plus the stream of outcomes in
/// completion order.
fn spawn_pool<T, F, Fut>(
    count: u32,
    concurrency: usize,
    task: F,
) -> (Vec<JoinHandle<()>>, mpsc::UnboundedReceiver<Outcome<T>>)
where
    T: Send + 'static,
    F: Fn(u32) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
{
    let next = Arc::new(AtomicU32::new(0));
    let (tx, rx) = mpsc::unbounded_channel();
    let handles = (0..concurrency)
        .map(|_| {
            let next = next.clone();
            let tx = tx.clone();
            let task = task.clone();
            tokio::spawn(async move {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let result = task(index).await;
                    if tx.send(Outcome { index, result }).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
    (handles, rx)
}

async fn upload_one(
    sdk: &Sdk,
    index: u32,
    size: u64,
    seed: u64,
    options: UploadOptions,
) -> Result<Uploaded> {
    let data = Data::new(seed, index, size);
    let start = Instant::now();
    let object = sdk
        .upload(Object::default(), data.reader(), options)
        .await
        .context("upload")?;
    Ok(Uploaded {
        index,
        object,
        upload: start.elapsed(),
    })
}

/// Pins an uploaded object's slabs and completes its record.
async fn pin_one(sdk: &Sdk, uploaded: Uploaded) -> Result<Record> {
    let Uploaded {
        index,
        object,
        upload,
    } = uploaded;
    let start = Instant::now();
    sdk.pin_object(&object).await.context("pin")?;
    Ok(Record {
        index,
        object_id: object.id(),
        size: object.size(),
        encoded_size: object.encoded_size(),
        upload,
        pin: start.elapsed(),
    })
}

async fn download_one(
    sdk: &Sdk,
    record: &Record,
    seed: u64,
    options: DownloadOptions,
) -> Result<Sample> {
    let data = Data::new(seed, record.index, record.size);
    // The verifier's clock starts here, so the object lookup the download needs
    // counts against the download the way it would for any client.
    let mut verifier = data.verifier();
    let object = sdk
        .object(&record.object_id)
        .await
        .context("object lookup")?;
    let mut download = sdk.download(&object, options).context("download")?;
    tokio::io::copy(&mut download, &mut verifier)
        .await
        .context("download")?;
    verifier.complete().context("verify")?;

    Ok(Sample {
        index: record.index,
        bytes: record.size,
        encoded: record.encoded_size,
        elapsed: verifier.elapsed(),
        ttfb: Some(verifier.ttfb()),
    })
}

/// Warns when the account cannot hold what the run is about to upload. The run
/// still starts; the indexer is the authority on its own limits.
async fn warn_on_quota(sdk: &Sdk, needed: u64) {
    match sdk.account().await {
        Ok(account) => {
            println!(
                "  {:<16}{} pinned, {} remaining",
                "Account",
                ByteSize(account.pinned_data),
                ByteSize(account.remaining_storage)
            );
            if account.remaining_storage < needed {
                eprintln!(
                    "warning: this run uploads {} but the account has {} remaining; later files will fail",
                    ByteSize(needed),
                    ByteSize(account.remaining_storage)
                );
            }
        }
        Err(e) => eprintln!("warning: could not read the account: {e}"),
    }
}

/// Uploads `count` synthetic files of `size` bytes, `concurrency` at a time,
/// and writes a manifest the `download` command can replay.
pub async fn upload(sdk: Sdk, args: UploadArgs) -> Result<()> {
    let UploadArgs {
        count,
        size,
        concurrency,
        manifest: manifest_path,
        seed,
        options,
    } = args;
    if count == 0 {
        bail!("count must be greater than zero");
    } else if size == 0 {
        bail!("size must be greater than zero");
    } else if concurrency == 0 {
        bail!("concurrency must be greater than zero");
    }
    options.validate().context("invalid upload options")?;

    let total_bytes = u64::from(count) * size;
    println!("Upload plan");
    println!("  {:<16}{count}", "Files");
    println!("  {:<16}{}", "File size", ByteSize(size));
    println!("  {:<16}{concurrency}", "Concurrency");
    println!("  {:<16}{}", "Total", ByteSize(total_bytes));
    println!("  {:<16}{seed}", "Seed");
    println!("  {:<16}{}", "Manifest", manifest_path.display());
    warn_on_quota(&sdk, total_bytes).await;

    let sampler = Sampler::start();
    let started = Instant::now();
    let (handles, mut outcomes) = spawn_pool(count, concurrency, {
        let sdk = sdk.clone();
        move |index| {
            let sdk = sdk.clone();
            let options = options.clone();
            async move { upload_one(&sdk, index, size, seed, options).await }
        }
    });

    // Pins run outside the pool so a worker starts its next file as soon as
    // the bytes land on hosts instead of waiting on the indexer.
    let mut pins = JoinSet::new();
    let mut manifest = Manifest::new(seed, size, concurrency);
    let mut samples = Vec::new();
    loop {
        tokio::select! {
            Some(outcome) = outcomes.recv() => match outcome.result {
                Ok(uploaded) => {
                    let sdk = sdk.clone();
                    pins.spawn(async move {
                        let index = uploaded.index;
                        let result = pin_one(&sdk, uploaded).await;
                        Outcome { index, result }
                    });
                }
                Err(e) => eprintln!("file {:>5}  FAILED: {e:#}", outcome.index),
            },
            Some(pinned) = pins.join_next() => match pinned {
                Ok(Outcome { result: Ok(record), .. }) => {
                    eprintln!(
                        "file {:>5}  uploaded {} in {} ({})",
                        record.index,
                        ByteSize(record.size),
                        units::duration(record.elapsed()),
                        units::bitrate(bits_per_second(record.size, record.elapsed()))
                    );
                    samples.push(Sample {
                        index: record.index,
                        bytes: record.size,
                        encoded: record.encoded_size,
                        elapsed: record.elapsed(),
                        ttfb: None,
                    });
                    manifest.files.push(record);
                    manifest.files.sort_by_key(|r| r.index);
                    manifest.save(&manifest_path)?;
                }
                Ok(Outcome { index, result: Err(e) }) => {
                    eprintln!("file {index:>5}  FAILED: {e:#}")
                }
                Err(e) => eprintln!("pin task failed: {e}"),
            },
            // Every worker has exited and every pin has landed.
            else => break,
        }
    }
    let wall = started.elapsed();
    for handle in handles {
        let _ = handle.await;
    }

    let summary = summarize(&samples, count as usize, concurrency, wall, sampler.stop());
    summary.print("Upload");
    println!("\nManifest written to {}", manifest_path.display());

    if summary.failed() > 0 {
        bail!(
            "{} of {} files failed to upload",
            summary.failed(),
            summary.attempted
        );
    }
    Ok(())
}

/// Downloads every file in the manifest, verifying each byte against the
/// synthetic data the upload generated.
pub async fn download(sdk: Sdk, args: DownloadArgs) -> Result<()> {
    let DownloadArgs {
        manifest: manifest_path,
        concurrency,
        options,
    } = args;
    let manifest = Manifest::load(&manifest_path)?;
    if manifest.files.is_empty() {
        bail!("manifest {} holds no files", manifest_path.display());
    }
    let concurrency = concurrency.unwrap_or(manifest.concurrency);
    if concurrency == 0 {
        bail!("concurrency must be greater than zero");
    }

    let total_bytes: u64 = manifest.files.iter().map(|r| r.size).sum();
    println!("Download plan");
    println!("  {:<16}{}", "Files", manifest.files.len());
    println!("  {:<16}{}", "File size", ByteSize(manifest.file_size));
    println!("  {:<16}{concurrency}", "Concurrency");
    println!("  {:<16}{}", "Total", ByteSize(total_bytes));
    println!("  {:<16}{}", "Manifest", manifest_path.display());

    // Failed uploads leave gaps in the manifest, so a worker's slot is a
    // position in the file list, not the file index the data was seeded with.
    let records = Arc::new(manifest.files);
    let attempted = records.len();
    let seed = manifest.seed;
    let sampler = Sampler::start();
    let started = Instant::now();
    let (handles, mut outcomes) = spawn_pool(attempted as u32, concurrency, {
        let records = records.clone();
        move |slot| {
            let sdk = sdk.clone();
            let options = options.clone();
            let records = records.clone();
            async move { download_one(&sdk, &records[slot as usize], seed, options).await }
        }
    });

    let mut samples = Vec::new();
    while let Some(outcome) = outcomes.recv().await {
        match outcome.result {
            Ok(sample) => {
                eprintln!(
                    "file {:>5}  downloaded {} in {} ({})",
                    sample.index,
                    ByteSize(sample.bytes),
                    units::duration(sample.elapsed),
                    units::bitrate(sample.bps())
                );
                samples.push(sample);
            }
            Err(e) => eprintln!(
                "file {:>5}  FAILED: {e:#}",
                records[outcome.index as usize].index
            ),
        }
    }
    let wall = started.elapsed();
    for handle in handles {
        let _ = handle.await;
    }

    let summary = summarize(&samples, attempted, concurrency, wall, sampler.stop());
    summary.print("Download");

    if summary.failed() > 0 {
        bail!(
            "{} of {} files failed to download",
            summary.failed(),
            summary.attempted
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// Drives `spawn_pool` with a task that records how many of its calls
    /// overlap, so the pool is checked without touching the network.
    #[tokio::test]
    async fn pool_respects_concurrency() {
        const COUNT: u32 = 40;
        const CONCURRENCY: usize = 4;

        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (handles, mut outcomes) = spawn_pool(COUNT, CONCURRENCY, {
            let (active, peak) = (active.clone(), peak.clone());
            move |index| {
                let (active, peak) = (active.clone(), peak.clone());
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    // Every third file fails, as a flaky host would.
                    if index % 3 == 0 {
                        bail!("boom {index}");
                    }
                    Ok(index)
                }
            }
        });

        let mut seen = Vec::new();
        let mut failed = Vec::new();
        while let Some(outcome) = outcomes.recv().await {
            match outcome.result {
                Ok(index) => seen.push(index),
                Err(_) => failed.push(outcome.index),
            }
        }
        for handle in handles {
            handle.await.unwrap();
        }

        seen.extend_from_slice(&failed);
        seen.sort_unstable();
        assert_eq!(seen, (0..COUNT).collect::<Vec<_>>(), "every file ran once");
        assert_eq!(failed.len(), COUNT.div_ceil(3) as usize);
        assert!(
            peak.load(Ordering::SeqCst) <= CONCURRENCY,
            "{} files ran at once, limit is {CONCURRENCY}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(active.load(Ordering::SeqCst), 0, "a worker leaked");
    }
}
