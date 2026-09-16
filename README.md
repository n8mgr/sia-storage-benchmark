# blog-benchmark

Upload and download benchmarks for Sia, driven through the `sia_storage` SDK
exactly as it ships. Grown from the SDK's own `examples/benchmark.rs`, with a
synthetic mode that transfers `n` files of `k` bytes with `m` transfers in
flight.

```sh
cargo build --release
./target/release/blog-benchmark login            # once, per indexer
./target/release/blog-benchmark run --size 1GiB  # one object, end to end
```

## Commands

| Command    | What it does                                                 |
|------------|--------------------------------------------------------------|
| `login`    | Runs the indexer's approval flow and stores the app key      |
| `run`      | Benchmarks one object: upload, pin, download, verify, delete |
| `upload`   | Uploads `n` synthetic files and writes a manifest            |
| `download` | Downloads and verifies every file in a manifest              |

`login --indexer <url>` picks the indexer; it defaults to `https://sia.storage`.
The app key and indexer are stored in the platform config directory (on macOS,
`~/Library/Application Support/tech.Sia.blog-benchmark/config.toml`).

Sizes accept unit suffixes: `KiB`, `MiB`, `GiB`, and `TiB` are binary; `KB`,
`MB`, `GB`, and `TB` are decimal; bare `K`, `M`, `G`, and `T` are binary.

### Synthetic runs

```sh
blog-benchmark upload   -n 500 -s 10GB -c 4 -m run.json
blog-benchmark download -m run.json
```

`upload` flags: `-n/--count`, `-s/--size`, `-c/--concurrency`,
`-m/--manifest`, `--seed`, `--upload-max-buffered-slabs`.

`download` flags: `-m/--manifest`, `-c/--concurrency` (defaults to whatever the
upload used), `--download-max-buffered-chunks`.

Both print the fastest, slowest, and average per-file speed, plus the aggregate
speed of the run:

```
Upload
  Files         500 of 500
  Concurrency   4
  Transferred   4.55 TiB (13.64 TiB encoded)
  Wall clock    3.21h
  Peak RSS      24.1 GiB of 64 GiB
  Aggregate     3.46 Gbps
  Fastest       982 Mbps (file 231, 1.36m)
  Slowest       311 Mbps (file 12, 4.29m)
  Average       855 Mbps
```

With `m` files in flight, the aggregate is the number that describes the link;
the per-file speeds describe a single transfer competing with `m - 1` others.

The exit code is non-zero if any file failed. `--seed` makes a run's data
reproducible; it is printed at startup either way.

## Methodology

- **Data.** File bytes come from an AES-CTR keystream keyed by the run seed and
  the file's index, so expected bytes are regenerated as a download streams in
  and no file is ever held in memory or on disk. The keystream is addressed by
  byte offset, so verification does not depend on how the SDK chunks the data.
- **Upload time** runs from the first byte offered to the SDK until the object
  is durable — `Sdk::upload` plus the `Sdk::pin_object` that follows it. The
  manifest keeps the two apart so the pin can be netted out. In `upload` the
  pin runs in the background while the slot moves on to its next file, so it
  never idles a transfer.
- **Download time** runs from the object lookup on the indexer through the last
  verified byte, which is what a client fetching by ID would experience.
- **TTFB** runs from that same origin until the first byte of object data
  reaches the application.
- **Speeds** are object size in bits over wall time, shown with SI prefixes
  (1 Mbps = 10^6 bits per second). The aggregate is total bytes over the run's
  wall clock; the average is the mean of the per-file speeds.
- **Memory** is the process's peak resident set, sampled once a second for the
  life of the run. `run` reports it separately for the upload and the
  download, since the two buffer differently.
- **Concurrency** is the number of whole files in flight. The SDK parallelizes
  *within* each transfer on its own — erasure-coded shards during upload,
  chunks during download — so `-c 1` is not a serial transfer.

Each transfer is driven through the SDK's own path; the benchmark does not
compose lower-level primitives to work around SDK behavior.

### Memory

`max_buffered_slabs` and `max_buffered_chunks` each default to 10% of system
memory **per transfer**, so `-c 4` can reach 40%. Cap them with
`--upload-max-buffered-slabs` and `--download-max-buffered-chunks` when running
many transfers at once. A slab is 120 MiB at the default 10-of-30 encoding.

Every run reports the peak resident set it reached, so the effect of those
flags is visible without watching the process.

### The manifest

`upload` writes a JSON manifest and rewrites it after every file completes, so
an interrupted run still leaves a manifest covering the files that made it. It
holds the run's seed and, per file, the object ID, sizes, and upload and pin
times. `download` needs it to know what to fetch and what bytes to expect, so a
download cannot be run without the matching upload's manifest.

Objects stay pinned after `download`; nothing deletes them. `run` is the only
command that cleans up after itself.

## Layout

Single crate. `main.rs` holds the CLI, `config.rs` the app key and the approval
flow, `bench.rs` the single-object `run`, and `synth.rs` the concurrent
`upload` and `download`. `data.rs` generates and verifies file bytes,
`manifest.rs` persists a run, `memory.rs` samples the resident set,
`stats.rs` aggregates it all, and `units.rs` formats sizes, speeds, and
durations.

A line goes to stderr as each file completes and the summary to stdout, so the
two can be redirected apart. Set `RUST_LOG` to capture the SDK's own logging,
which is written to a timestamped `benchmark-*.log` rather than the terminal.
