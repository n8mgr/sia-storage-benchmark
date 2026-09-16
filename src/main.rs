//! Upload and download benchmarks for the Sia storage SDK.

mod bench;
mod config;
mod data;
mod manifest;
mod memory;
mod stats;
mod synth;
mod units;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sia_storage::{DownloadOptions, UploadOptions};

use crate::units::ByteSize;

const DEFAULT_MANIFEST: &str = "manifest.json";

#[derive(Parser)]
#[command(
    name = "blog-benchmark",
    about = "Upload and download benchmarks for Sia",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Authorize this app against an indexer and store the resulting key, so
    /// later runs can skip the approval flow.
    Login {
        /// Indexer URL to authorize against.
        #[arg(long, default_value = config::DEFAULT_INDEXER)]
        indexer: String,

        /// Generate a new recovery phrase instead of prompting for one.
        #[arg(long)]
        new: bool,
    },
    /// Benchmark a single object end to end: upload, pin, download, verify,
    /// delete.
    Run {
        /// Size of the object to transfer.
        #[arg(short, long, default_value = "120MiB")]
        size: ByteSize,

        /// Run a smaller unmeasured roundtrip first to seed the SDK's upload
        /// and download metrics.
        #[arg(short, long)]
        warm: bool,

        /// Maximum number of slabs buffered in memory during upload.
        #[arg(long)]
        upload_max_buffered_slabs: Option<usize>,

        /// Maximum number of chunks buffered in memory during download.
        #[arg(long)]
        download_max_buffered_chunks: Option<usize>,

        /// Print a per-host breakdown of shards and throughput after the run.
        #[arg(long)]
        host_summary: bool,
    },
    /// Upload `count` synthetic files of `size` bytes with `concurrency`
    /// transfers in flight, and write a manifest for `download` to replay.
    Upload {
        /// Number of files to upload.
        #[arg(short = 'n', long, default_value_t = 10)]
        count: u32,

        /// Size of each file.
        #[arg(short, long, default_value = "1GiB")]
        size: ByteSize,

        /// Number of files transferred at the same time.
        #[arg(short, long, default_value_t = 1)]
        concurrency: usize,

        /// Where to write the manifest. Rewritten after every file completes.
        #[arg(short, long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,

        /// Seed for the generated data. Random when unset.
        #[arg(long)]
        seed: Option<u64>,

        /// Maximum number of slabs buffered in memory, per upload.
        #[arg(long)]
        upload_max_buffered_slabs: Option<usize>,
    },
    /// Download every file in a manifest and verify it byte for byte.
    Download {
        /// Manifest written by a previous `upload`.
        #[arg(short, long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,

        /// Number of files transferred at the same time. Defaults to whatever
        /// the upload used.
        #[arg(short, long)]
        concurrency: Option<usize>,

        /// Maximum number of chunks buffered in memory, per download.
        #[arg(long)]
        download_max_buffered_chunks: Option<usize>,
    },
}

/// Sends `RUST_LOG` output to a timestamped file so the SDK's logging does not
/// interleave with the benchmark's own output.
fn init_logging() -> Result<()> {
    let path = format!(
        "benchmark-{}.log",
        chrono::Local::now().format("%Y%m%dT%H%M%S")
    );
    let file = std::fs::File::create(&path).with_context(|| format!("failed to create {path}"))?;
    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();
    eprintln!("logging to {path}");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging()?;
    match cli.command {
        Command::Login { indexer, new } => config::login(&indexer, new).await,
        Command::Run {
            size,
            warm,
            upload_max_buffered_slabs,
            download_max_buffered_chunks,
            host_summary,
        } => {
            let sdk = config::connect().await?;
            bench::run(
                sdk,
                bench::RunArgs {
                    size: size.as_u64(),
                    warm,
                    upload: UploadOptions {
                        max_buffered_slabs: upload_max_buffered_slabs,
                        ..Default::default()
                    },
                    download: DownloadOptions {
                        max_buffered_chunks: download_max_buffered_chunks,
                        ..Default::default()
                    },
                    host_summary,
                },
            )
            .await
        }
        Command::Upload {
            count,
            size,
            concurrency,
            manifest,
            seed,
            upload_max_buffered_slabs,
        } => {
            let sdk = config::connect().await?;
            synth::upload(
                sdk,
                synth::UploadArgs {
                    count,
                    size: size.as_u64(),
                    concurrency,
                    manifest,
                    seed: seed.unwrap_or_else(rand::random),
                    options: UploadOptions {
                        max_buffered_slabs: upload_max_buffered_slabs,
                        ..Default::default()
                    },
                },
            )
            .await
        }
        Command::Download {
            manifest,
            concurrency,
            download_max_buffered_chunks,
        } => {
            let sdk = config::connect().await?;
            synth::download(
                sdk,
                synth::DownloadArgs {
                    manifest,
                    concurrency,
                    options: DownloadOptions {
                        max_buffered_chunks: download_max_buffered_chunks,
                        ..Default::default()
                    },
                },
            )
            .await
        }
    }
}
