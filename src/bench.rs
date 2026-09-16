//! The single-object benchmark: upload one object, pin it, download it back,
//! verify every byte, then delete it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use sia_storage::{DownloadOptions, Hash256, Object, Sdk, UploadOptions};

use crate::data::Data;
use crate::memory::{Sampler, Usage};
use crate::units::{self, ByteSize, bits_per_second};

/// The index the warmup object's data is seeded with, kept clear of the
/// measured object's.
const WARMUP_INDEX: u32 = u32::MAX;

pub struct RunArgs {
    pub size: u64,
    pub warm: bool,
    pub upload: UploadOptions,
    pub download: DownloadOptions,
    pub host_summary: bool,
}

/// What one host contributed to a transfer.
#[derive(Default)]
struct HostStat {
    shards: usize,
    bytes: u64,
    /// Summed per-shard transfer time. Shards to the same host can overlap, so
    /// this overcounts wall-clock; it estimates the per-connection rate.
    elapsed: Duration,
}

type HostStats = Arc<Mutex<HashMap<String, HostStat>>>;

struct Roundtrip {
    object_id: Hash256,
    size: u64,
    encoded_size: u64,
    upload: Duration,
    pin: Duration,
    download: Duration,
    ttfb: Duration,
    upload_memory: Usage,
    download_memory: Usage,
}

fn record_shard(stats: &HostStats, host: String, bytes: u64, elapsed: Duration) {
    let mut map = stats.lock().unwrap();
    let entry = map.entry(host).or_default();
    entry.shards += 1;
    entry.bytes += bytes;
    entry.elapsed += elapsed;
}

fn print_memory(usage: Usage) {
    if usage.peak == 0 {
        return;
    }
    println!(
        "  {:<15}{} of {}",
        "Peak RSS:",
        ByteSize(usage.peak),
        ByteSize(usage.system_total)
    );
}

fn print_host_summary(label: &str, stats: &HostStats) {
    let map = stats.lock().unwrap();
    if map.is_empty() {
        return;
    }
    let rate = |s: &HostStat| bits_per_second(s.bytes, s.elapsed);
    let mut rows: Vec<_> = map.iter().collect();
    rows.sort_by(|a, b| rate(b.1).total_cmp(&rate(a.1)));
    println!("\n{label} per-host summary ({} hosts):", map.len());
    for (host, s) in &rows {
        println!(
            "  {host}  {:>4} shards  {:>11}  {}",
            s.shards,
            ByteSize(s.bytes).to_string(),
            units::bitrate(rate(s))
        );
    }
    let total: u64 = map.values().map(|s| s.bytes).sum();
    println!("  total {} across {} hosts", ByteSize(total), map.len());
}

/// Uploads, pins, downloads, verifies, and deletes one object. The object is
/// deleted even when the download fails, so a failed run leaves nothing pinned.
async fn roundtrip(
    sdk: &Sdk,
    data: &Data,
    upload_options: UploadOptions,
    download_options: DownloadOptions,
    upload_hosts: Option<HostStats>,
    download_hosts: Option<HostStats>,
) -> Result<Roundtrip> {
    let upload_options = match upload_hosts {
        Some(stats) => upload_options.on_shard_uploaded(move |p| {
            record_shard(
                &stats,
                p.host_key.to_string(),
                p.shard_size as u64,
                p.elapsed,
            );
        }),
        None => upload_options,
    };

    let sampler = Sampler::start();
    let start = Instant::now();
    let object = sdk
        .upload(Object::default(), data.reader(), upload_options)
        .await
        .context("upload")?;
    let upload = start.elapsed();

    let start = Instant::now();
    sdk.pin_object(&object).await.context("pin")?;
    let pin = start.elapsed();
    let upload_memory = sampler.stop();

    let download_options = match download_hosts {
        Some(stats) => download_options.on_shard_downloaded(move |p| {
            record_shard(
                &stats,
                p.host_key.to_string(),
                p.shard_size as u64,
                p.elapsed,
            );
        }),
        None => download_options,
    };

    let sampler = Sampler::start();
    let mut verifier = data.verifier();
    let verified = async {
        let mut download = sdk.download(&object, download_options)?;
        tokio::io::copy(&mut download, &mut verifier).await?;
        verifier.complete()?;
        anyhow::Ok(())
    }
    .await;
    let download_memory = sampler.stop();

    let id = object.id();
    if let Err(e) = sdk.delete_object(&id).await {
        eprintln!("warning: failed to delete {id}: {e}");
    }
    verified.context("download")?;

    Ok(Roundtrip {
        object_id: id,
        size: object.size(),
        encoded_size: object.encoded_size(),
        upload,
        pin,
        download: verifier.elapsed(),
        ttfb: verifier.ttfb(),
        upload_memory,
        download_memory,
    })
}

/// Runs one measured roundtrip, optionally preceded by a smaller unmeasured one
/// that seeds the SDK's upload and download metrics.
pub async fn run(sdk: Sdk, args: RunArgs) -> Result<()> {
    args.upload.validate().context("invalid upload options")?;
    let seed: u64 = rand::random();

    if args.warm {
        // At least one full slab, or the upload pipeline never fills.
        let warm_size = (args.size / 10).max(args.upload.optimal_data_size() as u64);
        let data = Data::new(seed, WARMUP_INDEX, warm_size);
        roundtrip(
            &sdk,
            &data,
            args.upload.clone(),
            args.download.clone(),
            None,
            None,
        )
        .await
        .context("warmup")?;
    }

    let upload_hosts: HostStats = Arc::new(Mutex::new(HashMap::new()));
    let download_hosts: HostStats = Arc::new(Mutex::new(HashMap::new()));
    let data = Data::new(seed, 0, args.size);
    let result = roundtrip(
        &sdk,
        &data,
        args.upload,
        args.download,
        Some(upload_hosts.clone()),
        Some(download_hosts.clone()),
    )
    .await?;

    println!("\n{:<15}{}", "Object:", result.object_id);
    println!("{:<15}{}", "Size:", ByteSize(result.size));
    println!("{:<15}{}", "Encoded:", ByteSize(result.encoded_size));

    println!("\nUpload");
    println!("  {:<15}{}", "Elapsed:", units::duration(result.upload));
    println!("  {:<15}{}", "Pin:", units::duration(result.pin));
    println!(
        "  {:<15}{}",
        "Rate:",
        units::bitrate(bits_per_second(result.size, result.upload))
    );
    println!(
        "  {:<15}{}",
        "Encoded rate:",
        units::bitrate(bits_per_second(result.encoded_size, result.upload))
    );
    print_memory(result.upload_memory);

    println!("\nDownload");
    println!("  {:<15}{}", "Elapsed:", units::duration(result.download));
    println!("  {:<15}{}", "TTFB:", units::duration(result.ttfb));
    println!(
        "  {:<15}{}",
        "Rate:",
        units::bitrate(bits_per_second(result.size, result.download))
    );
    print_memory(result.download_memory);

    if args.host_summary {
        print_host_summary("Upload", &upload_hosts);
        print_host_summary("Download", &download_hosts);
    }
    Ok(())
}
