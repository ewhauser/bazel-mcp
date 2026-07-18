use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use bazel_mcp_bep::{
    DEFAULT_MAX_FRAME_BYTES, DEFAULT_MAX_STREAM_BYTES, DEFAULT_MAX_STREAM_EVENTS,
    IncrementalStreamDecoder, visit_stream_partial_borrowed_bounded,
};
use bazel_mcp_reducer::BepAccumulator;
use bazel_mcp_store::{InvocationCompletion, Store};
use bazel_mcp_types::{
    BazelCommand, InvocationId, InvocationRecord, InvocationRequest, InvocationState,
    InvocationSummary, PageRequest, Termination, TestResult, TestStatus,
};
use blake3::Hasher;
use clap::Parser;
use serde::{Deserialize, Serialize};

const PARALLEL_MMAP_HASH_THRESHOLD: u64 = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(about = "Reproducible end-to-end storage benchmark")]
struct Args {
    /// Backend/revision label recorded in the result.
    #[arg(long, default_value = "working-tree")]
    label: String,

    /// Git revision or other immutable source identifier recorded in the result.
    #[arg(long, default_value = "working-tree")]
    revision: String,

    /// Query rows written and indexed for the large-query workload.
    #[arg(long, default_value_t = 1_000_000)]
    query_rows: u64,

    /// Terminal invocations used for startup and garbage-collection workloads.
    #[arg(long, default_value_t = 2_000)]
    invocations: usize,

    /// Number of repeated point lookups used for latency sampling.
    #[arg(long, default_value_t = 1_000)]
    lookup_samples: usize,

    /// Lifecycle operations used for each concurrency level.
    #[arg(long, default_value_t = 320)]
    concurrency_operations: usize,

    /// Also measure startup with 100,000 retained manifests.
    #[arg(long, default_value_t = false)]
    extended_startup: bool,

    /// Optional JSON result path. The result is always printed to stdout.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Optional baseline JSON checked against broad regression gates.
    #[arg(long)]
    baseline: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    label: String,
    #[serde(default)]
    revision: String,
    platform: String,
    parameters: Parameters,
    #[serde(default)]
    lifecycle: LifecycleMetrics,
    #[serde(default)]
    concurrency: Vec<ConcurrencyMetrics>,
    #[serde(default)]
    bep_decode: BepDecodeMetrics,
    #[serde(default)]
    terminal: TerminalMetrics,
    query: QueryMetrics,
    startup: StartupMetrics,
    #[serde(default)]
    startup_scale: Vec<StartupMetrics>,
    gc: GcMetrics,
    #[serde(default)]
    process: ProcessMetrics,
}

#[derive(Debug, Deserialize, Serialize)]
struct Parameters {
    query_rows: u64,
    invocations: usize,
    lookup_samples: usize,
    #[serde(default)]
    concurrency_operations: usize,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LifecycleMetrics {
    elapsed_ms: f64,
    manifest_commits: u64,
    manifest_bytes_written: u64,
    payload_recounts: u64,
    retained_bytes: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ConcurrencyMetrics {
    concurrency: usize,
    operations: usize,
    throughput_per_second: f64,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
    latency_p99_ms: f64,
    manifest_commits: u64,
    manifest_bytes_written: u64,
    lookup_p95_us_during_writes: f64,
    inspection_p95_us_during_writes: f64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct BepDecodeMetrics {
    representative_bytes: u64,
    representative_events: usize,
    representative_decode_ms: f64,
    large_stream_bytes: u64,
    large_stream_events: usize,
    large_stream_decode_ms: f64,
    tailed_events: usize,
    tail_finalize_ms: f64,
    #[serde(default)]
    large_tailed_events: usize,
    #[serde(default)]
    large_tail_finalize_ms: f64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct TerminalMetrics {
    build_finalize_ms: f64,
    test_finalize_ms: f64,
    query_count_and_finalize_ms: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct QueryMetrics {
    stdout_bytes: u64,
    store_bytes: u64,
    postprocess_ms: f64,
    #[serde(default)]
    tail_count_finalize_ms: f64,
    unfiltered_page_ms: f64,
    #[serde(default)]
    continuation_page_ms: f64,
    filtered_page_ms: f64,
    lookup_p50_us: f64,
    lookup_p95_us: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct StartupMetrics {
    retained_invocations: usize,
    reopen_ms: f64,
    store_bytes: u64,
    #[serde(default)]
    directory_traversal_ms: f64,
    #[serde(default)]
    manifest_read_ms: f64,
    #[serde(default)]
    manifest_decode_ms: f64,
    #[serde(default)]
    index_build_ms: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct GcMetrics {
    candidates: usize,
    bytes_before: u64,
    target_bytes: u64,
    bytes_after: u64,
    deleted: usize,
    elapsed_ms: f64,
    #[serde(default)]
    lookup_p95_us_during_gc: f64,
    #[serde(default)]
    inspection_p95_us_during_gc: f64,
    #[serde(default)]
    renames: u64,
    #[serde(default)]
    unlinks: u64,
    #[serde(default)]
    rename_ms: f64,
    #[serde(default)]
    index_write_lock_ms: f64,
    #[serde(default)]
    unlink_ms: f64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ProcessMetrics {
    whole_benchmark_ms: f64,
    peak_rss_bytes: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let whole_started = Instant::now();
    let args = Args::parse();
    anyhow::ensure!(
        args.query_rows >= 200,
        "query_rows must be at least 200 to exercise continuation"
    );
    anyhow::ensure!(args.invocations > 1, "invocations must be greater than one");
    anyhow::ensure!(args.lookup_samples > 0, "lookup_samples must be positive");
    anyhow::ensure!(
        args.concurrency_operations >= 32,
        "concurrency_operations must be at least 32"
    );

    let lifecycle = benchmark_lifecycle().await?;
    let mut concurrency = Vec::new();
    for level in [1, 8, 32] {
        concurrency.push(benchmark_concurrency(level, args.concurrency_operations).await?);
    }
    let (query, query_terminal_ms) = benchmark_query(&args).await?;
    let terminal = benchmark_terminal(query_terminal_ms).await?;
    let startup = benchmark_startup(args.invocations).await?;
    let mut startup_scale = vec![benchmark_startup(args.invocations.saturating_mul(10)).await?];
    if args.extended_startup {
        startup_scale.push(benchmark_startup(100_000).await?);
    }
    let gc = benchmark_gc(args.invocations).await?;
    // Capture storage-workload RSS before the intentionally large synthetic
    // BEP experiment changes the process high-water mark.
    let storage_peak_rss_bytes = peak_rss_bytes();
    let bep_decode = benchmark_bep_decode()?;
    let process = ProcessMetrics {
        whole_benchmark_ms: millis(whole_started.elapsed()),
        peak_rss_bytes: storage_peak_rss_bytes,
    };
    let report = BenchmarkReport {
        schema_version: 4,
        label: args.label,
        revision: args.revision,
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        parameters: Parameters {
            query_rows: args.query_rows,
            invocations: args.invocations,
            lookup_samples: args.lookup_samples,
            concurrency_operations: args.concurrency_operations,
        },
        lifecycle,
        concurrency,
        bep_decode,
        terminal,
        query,
        startup,
        startup_scale,
        gc,
        process,
    };
    let json = serde_json::to_string_pretty(&report)?;
    if let Some(path) = args.output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create benchmark directory {}", parent.display()))?;
        }
        std::fs::write(&path, format!("{json}\n"))
            .with_context(|| format!("write benchmark result {}", path.display()))?;
    }
    println!("{json}");
    if let Some(path) = args.baseline {
        compare_baseline(&report, &path)?;
    }
    Ok(())
}

fn compare_baseline(current: &BenchmarkReport, path: &Path) -> anyhow::Result<()> {
    let baseline: BenchmarkReport = serde_json::from_slice(
        &std::fs::read(path)
            .with_context(|| format!("read benchmark baseline {}", path.display()))?,
    )?;
    anyhow::ensure!(
        current.parameters.query_rows == baseline.parameters.query_rows
            && current.parameters.invocations == baseline.parameters.invocations
            && current.parameters.lookup_samples == baseline.parameters.lookup_samples,
        "baseline workload parameters do not match"
    );
    anyhow::ensure!(
        current.query.store_bytes <= baseline.query.store_bytes.saturating_mul(101) / 100,
        "query storage exceeded the filesystem baseline by more than one percent"
    );
    anyhow::ensure!(
        current.query.postprocess_ms <= baseline.query.postprocess_ms * 0.25,
        "query post-processing exceeded 25% of baseline"
    );
    anyhow::ensure!(
        current.query.filtered_page_ms <= baseline.query.filtered_page_ms * 1.25,
        "filtered query pagination exceeded the filesystem baseline by 25%"
    );
    anyhow::ensure!(
        current.query.lookup_p95_us <= baseline.query.lookup_p95_us * 2.0,
        "point lookup p95 exceeded twice the filesystem baseline"
    );
    anyhow::ensure!(
        current.startup.reopen_ms <= baseline.startup.reopen_ms * 1.5,
        "startup rebuild exceeded the filesystem baseline by 50%"
    );
    anyhow::ensure!(
        current.gc.elapsed_ms <= baseline.gc.elapsed_ms * 1.5,
        "quota GC exceeded the filesystem baseline by 50%"
    );
    anyhow::ensure!(
        current.gc.bytes_after <= current.gc.target_bytes,
        "quota GC did not reach its target"
    );
    anyhow::ensure!(
        current.lifecycle.manifest_commits <= 4,
        "a lifecycle used more than four manifest commits"
    );
    anyhow::ensure!(
        current.lifecycle.payload_recounts <= 2,
        "a lifecycle recounted evidence more than twice"
    );
    anyhow::ensure!(
        current
            .concurrency
            .iter()
            .map(|item| item.concurrency)
            .collect::<Vec<_>>()
            == [1, 8, 32],
        "concurrency measurements must cover 1, 8, and 32 writers"
    );
    anyhow::ensure!(
        current.gc.renames == current.gc.deleted as u64
            && current.gc.unlinks == current.gc.deleted as u64,
        "GC did not rename and unlink every deleted candidate"
    );
    let single = &current.concurrency[0];
    let eight = &current.concurrency[1];
    let thirty_two = &current.concurrency[2];
    anyhow::ensure!(
        eight.throughput_per_second >= single.throughput_per_second * 0.9,
        "eight-writer throughput materially regressed from one writer"
    );
    anyhow::ensure!(
        thirty_two.throughput_per_second >= single.throughput_per_second * 0.7,
        "32-writer throughput materially regressed from one writer"
    );
    anyhow::ensure!(
        current.terminal.build_finalize_ms <= 10.0
            && current.terminal.test_finalize_ms <= 10.0
            && current.terminal.query_count_and_finalize_ms <= 20.0,
        "representative terminal finalization exceeded its broad latency gate"
    );
    anyhow::ensure!(
        current.bep_decode.representative_decode_ms <= 5.0,
        "representative post-exit BEP reduction exceeded five milliseconds"
    );
    anyhow::ensure!(
        current.gc.lookup_p95_us_during_gc <= 100.0
            && current.gc.inspection_p95_us_during_gc <= 100.0,
        "point lookups or inspections stalled during GC"
    );
    Ok(())
}

fn benchmark_bep_decode() -> anyhow::Result<BepDecodeMetrics> {
    let representative = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../bazel-mcp-reducer/tests/fixtures/bazel-9/test-outcomes.bep"),
    )?;
    let (representative_events, representative_decode_ms) = decode_bep(&representative)?;

    const LARGE_STREAM_TARGET_BYTES: usize = 64 * 1024 * 1024;
    let repetitions = LARGE_STREAM_TARGET_BYTES / representative.len();
    let mut large = Vec::with_capacity(repetitions * representative.len());
    for _ in 0..repetitions {
        large.extend_from_slice(&representative);
    }
    let (large_stream_events, large_stream_decode_ms) = decode_bep(&large)?;
    let (tailed_events, tail_finalize_ms) = tail_bep_file(&representative, 1_021)?;
    let (large_tailed_events, large_tail_finalize_ms) = tail_bep_file(&large, 64 * 1024 + 13)?;
    Ok(BepDecodeMetrics {
        representative_bytes: representative.len() as u64,
        representative_events,
        representative_decode_ms,
        large_stream_bytes: large.len() as u64,
        large_stream_events,
        large_stream_decode_ms,
        tailed_events,
        tail_finalize_ms,
        large_tailed_events,
        large_tail_finalize_ms,
    })
}

fn tail_bep_file(bytes: &[u8], chunk_bytes: usize) -> anyhow::Result<(usize, f64)> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("events.bep");
    std::fs::File::create(&path)?;
    let done = Arc::new(AtomicBool::new(false));
    let observed_bytes = Arc::new(AtomicU64::new(0));
    let tail_done = done.clone();
    let tail_observed_bytes = observed_bytes.clone();
    let tail_path = path.clone();
    let tailer = std::thread::spawn(move || {
        tail_complete_bep_frames(&tail_path, &tail_done, &tail_observed_bytes)
    });
    let mut writer = std::fs::OpenOptions::new().append(true).open(path)?;
    for chunk in bytes.chunks(chunk_bytes) {
        writer.write_all(chunk)?;
        writer.flush()?;
        std::thread::yield_now();
    }
    drop(writer);
    let catchup_started = Instant::now();
    while observed_bytes.load(Ordering::Acquire) < bytes.len() as u64 {
        anyhow::ensure!(
            catchup_started.elapsed() < Duration::from_secs(5),
            "incremental BEP tail did not catch up"
        );
        std::thread::yield_now();
    }
    let finalized = Instant::now();
    done.store(true, Ordering::Release);
    let (events, tailed_bytes, tailed_digest) = tailer
        .join()
        .map_err(|_| anyhow::anyhow!("BEP tail experiment panicked"))??;
    let (final_bytes, final_digest) = hash_bep_file(root.path().join("events.bep"))?;
    anyhow::ensure!(
        tailed_bytes == final_bytes && tailed_digest == final_digest,
        "tailed BEP bytes did not match retained file"
    );
    Ok((events, millis(finalized.elapsed())))
}

fn tail_complete_bep_frames(
    path: &Path,
    done: &AtomicBool,
    observed_bytes: &AtomicU64,
) -> anyhow::Result<(usize, u64, [u8; 32])> {
    let mut reader = std::fs::File::open(path)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut accumulator = BepAccumulator::default();
    let mut decoder = IncrementalStreamDecoder::new(
        DEFAULT_MAX_FRAME_BYTES,
        DEFAULT_MAX_STREAM_BYTES,
        DEFAULT_MAX_STREAM_EVENTS,
    );
    let mut hasher = Hasher::new();
    let mut tailed_bytes = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read > 0 {
            hasher.update(&buffer[..read]);
            tailed_bytes = tailed_bytes.saturating_add(read as u64);
            decoder.push_borrowed(&buffer[..read], |event| accumulator.observe_borrowed(event));
            observed_bytes.store(tailed_bytes, Ordering::Release);
            continue;
        }
        if done.load(Ordering::Acquire) {
            let outcome = decoder.finish();
            anyhow::ensure!(
                outcome.terminal_error.is_none(),
                "tailed BEP decode failed: {:?}",
                outcome.terminal_error
            );
            std::hint::black_box(accumulator);
            return Ok((
                outcome.event_count,
                tailed_bytes,
                *hasher.finalize().as_bytes(),
            ));
        }
        std::thread::yield_now();
    }
}

fn hash_bep_file(path: PathBuf) -> anyhow::Result<(u64, [u8; 32])> {
    let bytes = std::fs::metadata(&path)?.len();
    let mut hasher = Hasher::new();
    if bytes >= PARALLEL_MMAP_HASH_THRESHOLD {
        hasher.update_mmap_rayon(path)?;
    } else {
        let mut reader = File::open(path)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }
    Ok((bytes, *hasher.finalize().as_bytes()))
}

fn decode_bep(bytes: &[u8]) -> anyhow::Result<(usize, f64)> {
    let mut accumulator = BepAccumulator::default();
    let started = Instant::now();
    let outcome = visit_stream_partial_borrowed_bounded(
        std::io::Cursor::new(bytes),
        DEFAULT_MAX_FRAME_BYTES,
        DEFAULT_MAX_STREAM_BYTES,
        DEFAULT_MAX_STREAM_EVENTS,
        |event| accumulator.observe_borrowed(event),
    );
    let elapsed_ms = millis(started.elapsed());
    anyhow::ensure!(
        outcome.terminal_error.is_none(),
        "BEP decode failed: {:?}",
        outcome.terminal_error
    );
    std::hint::black_box(accumulator);
    Ok((outcome.event_count, elapsed_ms))
}

async fn benchmark_lifecycle() -> anyhow::Result<LifecycleMetrics> {
    let root = tempfile::tempdir()?;
    let store = Store::open(root.path()).await?;
    let before = store.io_stats();
    let started = Instant::now();
    create_terminal_invocation(&store, 0, 4 * 1024).await?;
    let elapsed_ms = millis(started.elapsed());
    let after = store.io_stats();
    Ok(LifecycleMetrics {
        elapsed_ms,
        manifest_commits: after.manifest_commits - before.manifest_commits,
        manifest_bytes_written: after.manifest_bytes_written - before.manifest_bytes_written,
        payload_recounts: after.payload_recounts - before.payload_recounts,
        retained_bytes: directory_size(root.path())?,
    })
}

async fn benchmark_concurrency(
    concurrency: usize,
    operations: usize,
) -> anyhow::Result<ConcurrencyMetrics> {
    let root = tempfile::tempdir()?;
    let store = Store::open(root.path()).await?;
    let inspect_id = create_terminal_invocation(&store, usize::MAX, 0).await?;
    let before = store.io_stats();
    let permits = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(operations);
    for ordinal in 0..operations {
        let store = store.clone();
        let permits = permits.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permits.acquire_owned().await?;
            let operation_started = Instant::now();
            create_terminal_invocation(&store, ordinal, 0)
                .await
                .with_context(|| format!("create concurrent invocation {ordinal}"))?;
            Ok::<Duration, anyhow::Error>(operation_started.elapsed())
        }));
    }
    let mut lookup_latencies = Vec::new();
    let mut inspection_latencies = Vec::new();
    while tasks.iter().any(|task| !task.is_finished()) && lookup_latencies.len() < 100_000 {
        let lookup_started = Instant::now();
        store
            .get_invocation_header(inspect_id)
            .await
            .with_context(|| format!("look up inspection seed {inspect_id}"))?;
        lookup_latencies.push(lookup_started.elapsed());
        let inspection_started = Instant::now();
        store
            .page_diagnostics(inspect_id, None, PageRequest::default())
            .await
            .with_context(|| format!("inspect diagnostics for seed {inspect_id}"))?;
        inspection_latencies.push(inspection_started.elapsed());
    }
    if lookup_latencies.is_empty() {
        let lookup_started = Instant::now();
        store
            .get_invocation_header(inspect_id)
            .await
            .with_context(|| format!("look up inspection seed {inspect_id}"))?;
        lookup_latencies.push(lookup_started.elapsed());
        let inspection_started = Instant::now();
        store
            .page_diagnostics(inspect_id, None, PageRequest::default())
            .await
            .with_context(|| format!("inspect diagnostics for seed {inspect_id}"))?;
        inspection_latencies.push(inspection_started.elapsed());
    }
    let mut latencies = Vec::with_capacity(operations);
    for task in tasks {
        latencies.push(task.await??);
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    lookup_latencies.sort_unstable();
    inspection_latencies.sort_unstable();
    let after = store.io_stats();
    Ok(ConcurrencyMetrics {
        concurrency,
        operations,
        throughput_per_second: operations as f64 / elapsed.as_secs_f64(),
        latency_p50_ms: millis(percentile(&latencies, 50)),
        latency_p95_ms: millis(percentile(&latencies, 95)),
        latency_p99_ms: millis(percentile(&latencies, 99)),
        manifest_commits: after.manifest_commits - before.manifest_commits,
        manifest_bytes_written: after.manifest_bytes_written - before.manifest_bytes_written,
        lookup_p95_us_during_writes: micros(percentile(&lookup_latencies, 95)),
        inspection_p95_us_during_writes: micros(percentile(&inspection_latencies, 95)),
    })
}

async fn create_terminal_invocation(
    store: &Store,
    ordinal: usize,
    evidence_bytes: usize,
) -> anyhow::Result<InvocationId> {
    let request = InvocationRequest::new(
        PathBuf::from(format!("/benchmark/concurrent-workspace-{}", ordinal % 16)),
        BazelCommand::Build,
        vec![format!("//benchmark:concurrent-target-{ordinal}")],
    );
    let id = request.id;
    let paths = store
        .create_invocation(&InvocationRecord::queued(request))
        .await
        .with_context(|| format!("create invocation {id}"))?;
    store
        .transition(id, InvocationState::Starting, None, None)
        .await
        .with_context(|| format!("start invocation {id}"))?;
    store
        .transition(id, InvocationState::Running, None, None)
        .await
        .with_context(|| format!("run invocation {id}"))?;
    if evidence_bytes > 0 {
        std::fs::write(paths.stdout, vec![b'x'; evidence_bytes])?;
    }
    store
        .finish_invocation(
            id,
            InvocationCompletion {
                state: InvocationState::Succeeded,
                termination: Termination::Exit { code: 0 },
                summary: InvocationSummary {
                    success: true,
                    headline: "benchmark invocation succeeded".to_owned(),
                    ..InvocationSummary::default()
                },
                run: None,
                metrics: Default::default(),
                canonical_arguments: Some(vec![
                    "build".into(),
                    format!("//benchmark:concurrent-target-{ordinal}"),
                ]),
                artifacts: Vec::new(),
            },
        )
        .await
        .with_context(|| format!("finish invocation {id}"))?;
    Ok(id)
}

async fn benchmark_query(args: &Args) -> anyhow::Result<(QueryMetrics, f64)> {
    let root = tempfile::tempdir()?;
    let store = Store::open(root.path()).await?;
    let request = InvocationRequest::new(
        PathBuf::from("/benchmark/workspace"),
        BazelCommand::Query,
        vec!["//...".to_owned()],
    );
    let id = request.id;
    let paths = store
        .create_invocation(&InvocationRecord::queued(request))
        .await?;
    store
        .transition(id, InvocationState::Starting, None, None)
        .await?;
    store
        .transition(id, InvocationState::Running, None, None)
        .await?;
    let (tail_count, tail_count_finalize_ms) =
        write_query_fixture_with_tail(&paths.stdout, args.query_rows)?;
    anyhow::ensure!(
        tail_count == args.query_rows,
        "tailed query row count mismatch"
    );
    let stdout_bytes = std::fs::metadata(&paths.stdout)?.len();

    let postprocess_started = Instant::now();
    let total = store.count_query_rows(id).await?;
    let sample = store
        .page_query_rows(
            id,
            None,
            PageRequest {
                scan_limit: 3,
                ..PageRequest::new(None, 3)
            },
        )
        .await?;
    let postprocess_ms = millis(postprocess_started.elapsed());
    anyhow::ensure!(sample.items.len() == 3, "unexpected query sample length");
    anyhow::ensure!(total == args.query_rows, "wrong query row count");
    store
        .transition(
            id,
            InvocationState::Succeeded,
            Some(Termination::Exit { code: 0 }),
            Some(InvocationSummary {
                success: true,
                headline: format!("Bazel query returned {} rows", args.query_rows),
                query_result_count: Some(args.query_rows),
                ..InvocationSummary::default()
            }),
        )
        .await?;
    let terminal_ms = millis(postprocess_started.elapsed());

    let page_started = Instant::now();
    let page = store
        .page_query_rows(id, None, PageRequest::new(None, 100))
        .await?;
    let unfiltered_page_ms = millis(page_started.elapsed());
    anyhow::ensure!(page.items.len() == 100, "unexpected query page length");
    anyhow::ensure!(
        page.total_count == Some(args.query_rows) && page.filtered_count == page.total_count,
        "wrong query counts"
    );

    let continuation_started = Instant::now();
    let continued = store
        .page_query_rows(id, None, PageRequest::new(page.next_cursor, 100))
        .await?;
    let continuation_page_ms = millis(continuation_started.elapsed());
    anyhow::ensure!(
        continued.items.len() == 100,
        "unexpected continuation length"
    );
    anyhow::ensure!(
        continued.total_count == Some(args.query_rows)
            && continued.filtered_count == continued.total_count,
        "wrong continuation counts"
    );

    let last_value = format!("//benchmark/package:target_{:012}", args.query_rows - 1);
    let filtered_started = Instant::now();
    let page = store
        .page_query_rows(
            id,
            Some(&last_value),
            PageRequest {
                scan_limit: u32::MAX,
                ..PageRequest::new(None, 100)
            },
        )
        .await?;
    let filtered_page_ms = millis(filtered_started.elapsed());
    anyhow::ensure!(
        page.items.len() == 1 && page.filtered_count == Some(1),
        "filtered query mismatch"
    );

    let mut lookups = Vec::with_capacity(args.lookup_samples);
    for _ in 0..args.lookup_samples {
        let started = Instant::now();
        let record = store.get_invocation_header(id).await?;
        lookups.push(started.elapsed());
        anyhow::ensure!(
            record.request.id == id,
            "point lookup returned wrong record"
        );
    }
    lookups.sort_unstable();
    let store_bytes = directory_size(root.path())?;
    Ok((
        QueryMetrics {
            stdout_bytes,
            store_bytes,
            postprocess_ms,
            tail_count_finalize_ms,
            unfiltered_page_ms,
            continuation_page_ms,
            filtered_page_ms,
            lookup_p50_us: micros(percentile(&lookups, 50)),
            lookup_p95_us: micros(percentile(&lookups, 95)),
        },
        terminal_ms,
    ))
}

async fn benchmark_terminal(query_count_and_finalize_ms: f64) -> anyhow::Result<TerminalMetrics> {
    let build_root = tempfile::tempdir()?;
    let build_store = Store::open(build_root.path()).await?;
    let build_id = create_running_invocation(&build_store, BazelCommand::Build).await?;
    let started = Instant::now();
    build_store
        .finish_invocation(
            build_id,
            InvocationCompletion {
                state: InvocationState::Succeeded,
                termination: Termination::Exit { code: 0 },
                summary: InvocationSummary {
                    success: true,
                    headline: "representative build succeeded".to_owned(),
                    ..InvocationSummary::default()
                },
                run: None,
                metrics: Default::default(),
                canonical_arguments: Some(vec!["build".into(), "//benchmark:build".into()]),
                artifacts: Vec::new(),
            },
        )
        .await?;
    let build_finalize_ms = millis(started.elapsed());

    let test_root = tempfile::tempdir()?;
    let test_store = Store::open(test_root.path()).await?;
    let test_id = create_running_invocation(&test_store, BazelCommand::Test).await?;
    let tests = (0..500)
        .map(|ordinal| TestResult {
            label: format!("//benchmark:test_{ordinal}"),
            status: TestStatus::Passed,
            duration_ms: Some(10),
            attempts: 1,
            shard: None,
            cases: Vec::new(),
            test_log_available: false,
            test_log_unavailable_reason: None,
        })
        .collect();
    let started = Instant::now();
    test_store
        .finish_invocation(
            test_id,
            InvocationCompletion {
                state: InvocationState::Succeeded,
                termination: Termination::Exit { code: 0 },
                summary: InvocationSummary {
                    success: true,
                    headline: "representative tests succeeded".to_owned(),
                    tests,
                    ..InvocationSummary::default()
                },
                run: None,
                metrics: Default::default(),
                canonical_arguments: Some(vec!["test".into(), "//benchmark:all".into()]),
                artifacts: Vec::new(),
            },
        )
        .await?;
    let test_finalize_ms = millis(started.elapsed());

    Ok(TerminalMetrics {
        build_finalize_ms,
        test_finalize_ms,
        query_count_and_finalize_ms,
    })
}

async fn create_running_invocation(
    store: &Store,
    command: BazelCommand,
) -> anyhow::Result<InvocationId> {
    let request = InvocationRequest::new(
        PathBuf::from("/benchmark/terminal-workspace"),
        command,
        vec!["//benchmark:all".to_owned()],
    );
    let id = request.id;
    store
        .create_invocation(&InvocationRecord::queued(request))
        .await?;
    store
        .transition(id, InvocationState::Starting, None, None)
        .await?;
    store
        .transition(id, InvocationState::Running, None, None)
        .await?;
    Ok(id)
}

async fn benchmark_startup(invocations: usize) -> anyhow::Result<StartupMetrics> {
    let root = tempfile::tempdir()?;
    populate_terminal_invocations(root.path(), invocations, 0).await?;
    let store_bytes = directory_size(root.path())?;
    let started = Instant::now();
    let reopened = Store::open(root.path()).await?;
    let reopen_ms = millis(started.elapsed());
    let decomposition = reopened.startup_stats();
    drop(reopened);
    Ok(StartupMetrics {
        retained_invocations: invocations,
        reopen_ms,
        store_bytes,
        directory_traversal_ms: decomposition.directory_traversal_us as f64 / 1_000.0,
        manifest_read_ms: decomposition.manifest_read_us as f64 / 1_000.0,
        manifest_decode_ms: decomposition.manifest_decode_us as f64 / 1_000.0,
        index_build_ms: decomposition.index_build_us as f64 / 1_000.0,
    })
}

async fn benchmark_gc(invocations: usize) -> anyhow::Result<GcMetrics> {
    let root = tempfile::tempdir()?;
    let store = populate_terminal_invocations(root.path(), invocations, 4 * 1024).await?;
    let live = InvocationRequest::new(
        PathBuf::from("/benchmark/live-during-gc"),
        BazelCommand::Build,
        vec!["//benchmark:live-during-gc".into()],
    );
    let live_id = live.id;
    store
        .create_invocation(&InvocationRecord::queued(live))
        .await?;
    let bytes_before = directory_size(root.path())?;
    let target_bytes = bytes_before / 2;
    let before_stats = store.io_stats();
    let running = Arc::new(AtomicBool::new(true));
    let gc_store = store.clone();
    let gc_running = running.clone();
    let started = Instant::now();
    let gc = tokio::spawn(async move {
        let result = gc_store
            .enforce_retention(Duration::from_secs(365 * 24 * 60 * 60), target_bytes)
            .await;
        gc_running.store(false, Ordering::Release);
        result
    });
    let mut lookups = Vec::new();
    let mut inspections = Vec::new();
    while running.load(Ordering::Acquire) && lookups.len() < 100_000 {
        let lookup_started = Instant::now();
        store.get_invocation_header(live_id).await?;
        lookups.push(lookup_started.elapsed());
        let inspection_started = Instant::now();
        store
            .page_diagnostics(live_id, None, PageRequest::default())
            .await?;
        inspections.push(inspection_started.elapsed());
    }
    if lookups.is_empty() {
        let lookup_started = Instant::now();
        store.get_invocation_header(live_id).await?;
        lookups.push(lookup_started.elapsed());
        let inspection_started = Instant::now();
        store
            .page_diagnostics(live_id, None, PageRequest::default())
            .await?;
        inspections.push(inspection_started.elapsed());
    }
    let deleted = gc.await??;
    let elapsed_ms = millis(started.elapsed());
    lookups.sort_unstable();
    inspections.sort_unstable();
    let bytes_after = directory_size(root.path())?;
    let after_stats = store.io_stats();
    Ok(GcMetrics {
        candidates: invocations,
        bytes_before,
        target_bytes,
        bytes_after,
        deleted,
        elapsed_ms,
        lookup_p95_us_during_gc: micros(percentile(&lookups, 95)),
        inspection_p95_us_during_gc: micros(percentile(&inspections, 95)),
        renames: after_stats.gc_renames - before_stats.gc_renames,
        unlinks: after_stats.gc_unlinks - before_stats.gc_unlinks,
        rename_ms: (after_stats.gc_rename_us - before_stats.gc_rename_us) as f64 / 1_000.0,
        index_write_lock_ms: (after_stats.gc_index_write_us - before_stats.gc_index_write_us)
            as f64
            / 1_000.0,
        unlink_ms: (after_stats.gc_unlink_us - before_stats.gc_unlink_us) as f64 / 1_000.0,
    })
}

async fn populate_terminal_invocations(
    root: &Path,
    count: usize,
    evidence_bytes: usize,
) -> anyhow::Result<Store> {
    let store = Store::open(root).await?;
    for ordinal in 0..count {
        create_terminal_invocation(&store, ordinal, evidence_bytes).await?;
    }
    Ok(store)
}

fn write_query_fixture_with_tail(path: &Path, rows: u64) -> anyhow::Result<(u64, f64)> {
    File::create(path)?;
    let done = Arc::new(AtomicBool::new(false));
    let tail_done = done.clone();
    let tail_path = path.to_owned();
    let tailer = std::thread::spawn(move || tail_query_rows(&tail_path, &tail_done));
    let mut writer = BufWriter::new(std::fs::OpenOptions::new().append(true).open(path)?);
    for ordinal in 0..rows {
        writeln!(writer, "//benchmark/package:target_{ordinal:012}")?;
    }
    writer.flush()?;
    let finalized = Instant::now();
    done.store(true, Ordering::Release);
    let count = tailer
        .join()
        .map_err(|_| anyhow::anyhow!("query tail experiment panicked"))??;
    Ok((count, millis(finalized.elapsed())))
}

fn tail_query_rows(path: &Path, done: &AtomicBool) -> anyhow::Result<u64> {
    use std::io::Read;

    let mut reader = File::open(path)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut rows = 0_u64;
    let mut saw_bytes = false;
    let mut last_byte = b'\n';
    loop {
        let read = reader.read(&mut buffer)?;
        if read > 0 {
            saw_bytes = true;
            last_byte = buffer[read - 1];
            rows = rows.saturating_add(
                u64::try_from(memchr::memchr_iter(b'\n', &buffer[..read]).count())
                    .unwrap_or(u64::MAX),
            );
            continue;
        }
        if done.load(Ordering::Acquire) {
            if saw_bytes && last_byte != b'\n' {
                rows = rows.saturating_add(1);
            }
            return Ok(rows);
        }
        std::thread::yield_now();
    }
}

fn directory_size(root: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

fn percentile(samples: &[Duration], percentile: usize) -> Duration {
    let index = (samples.len().saturating_sub(1) * percentile) / 100;
    samples[index]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for `getrusage`, and is only
    // assumed initialized after the operating system reports success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: a successful `getrusage` call initialized the entire structure.
    let raw = unsafe { usage.assume_init() }.ru_maxrss;
    let raw = u64::try_from(raw).ok()?;
    #[cfg(target_os = "macos")]
    return Some(raw);
    #[cfg(not(target_os = "macos"))]
    return Some(raw.saturating_mul(1024));
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}
