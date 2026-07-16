use std::{
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Context;
use bazel_mcp_store::Store;
use bazel_mcp_types::{
    BazelCommand, InvocationRecord, InvocationRequest, InvocationState, InvocationSummary,
    PageRequest, Termination,
};
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(about = "Reproducible end-to-end storage benchmark")]
struct Args {
    /// Backend/revision label recorded in the result.
    #[arg(long, default_value = "working-tree")]
    label: String,

    /// Query rows written and indexed for the large-query workload.
    #[arg(long, default_value_t = 1_000_000)]
    query_rows: u64,

    /// Terminal invocations used for startup and garbage-collection workloads.
    #[arg(long, default_value_t = 2_000)]
    invocations: usize,

    /// Number of repeated point lookups used for latency sampling.
    #[arg(long, default_value_t = 1_000)]
    lookup_samples: usize,

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
    platform: String,
    parameters: Parameters,
    query: QueryMetrics,
    startup: StartupMetrics,
    gc: GcMetrics,
}

#[derive(Debug, Deserialize, Serialize)]
struct Parameters {
    query_rows: u64,
    invocations: usize,
    lookup_samples: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct QueryMetrics {
    stdout_bytes: u64,
    store_bytes: u64,
    postprocess_ms: f64,
    unfiltered_page_ms: f64,
    filtered_page_ms: f64,
    lookup_p50_us: f64,
    lookup_p95_us: f64,
}

#[derive(Debug, Deserialize, Serialize)]
struct StartupMetrics {
    retained_invocations: usize,
    reopen_ms: f64,
    store_bytes: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct GcMetrics {
    candidates: usize,
    bytes_before: u64,
    target_bytes: u64,
    bytes_after: u64,
    deleted: usize,
    elapsed_ms: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    anyhow::ensure!(args.query_rows > 0, "query_rows must be positive");
    anyhow::ensure!(args.invocations > 1, "invocations must be greater than one");
    anyhow::ensure!(args.lookup_samples > 0, "lookup_samples must be positive");

    let query = benchmark_query(&args).await?;
    let startup = benchmark_startup(args.invocations).await?;
    let gc = benchmark_gc(args.invocations).await?;
    let report = BenchmarkReport {
        schema_version: 1,
        label: args.label,
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        parameters: Parameters {
            query_rows: args.query_rows,
            invocations: args.invocations,
            lookup_samples: args.lookup_samples,
        },
        query,
        startup,
        gc,
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
        current.query.store_bytes <= baseline.query.store_bytes / 2,
        "query storage exceeded half the baseline"
    );
    anyhow::ensure!(
        current.query.postprocess_ms <= baseline.query.postprocess_ms * 0.25,
        "query post-processing exceeded 25% of baseline"
    );
    anyhow::ensure!(
        current.query.filtered_page_ms <= baseline.query.filtered_page_ms * 0.5,
        "filtered query pagination exceeded 50% of baseline"
    );
    anyhow::ensure!(
        current.query.lookup_p95_us <= baseline.query.lookup_p95_us * 0.25,
        "point lookup p95 exceeded 25% of baseline"
    );
    anyhow::ensure!(
        current.startup.reopen_ms <= baseline.startup.reopen_ms * 2.5,
        "startup rebuild exceeded 2.5x baseline"
    );
    anyhow::ensure!(
        current.gc.elapsed_ms <= baseline.gc.elapsed_ms * 0.75,
        "quota GC exceeded 75% of baseline"
    );
    anyhow::ensure!(
        current.gc.bytes_after <= current.gc.target_bytes,
        "quota GC did not reach its target"
    );
    Ok(())
}

async fn benchmark_query(args: &Args) -> anyhow::Result<QueryMetrics> {
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
    write_query_fixture(&paths.stdout, args.query_rows)?;
    let stdout_bytes = std::fs::metadata(&paths.stdout)?.len();

    let postprocess_started = Instant::now();
    let (sample, total, _) = store
        .page_query_rows(
            id,
            None,
            PageRequest {
                cursor: None,
                limit: 3,
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

    let page_started = Instant::now();
    let (page, total, filtered) = store
        .page_query_rows(
            id,
            None,
            PageRequest {
                cursor: None,
                limit: 100,
            },
        )
        .await?;
    let unfiltered_page_ms = millis(page_started.elapsed());
    anyhow::ensure!(page.items.len() == 100, "unexpected query page length");
    anyhow::ensure!(
        total == args.query_rows && filtered == total,
        "wrong query counts"
    );

    let last_value = format!("//benchmark/package:target_{:012}", args.query_rows - 1);
    let filtered_started = Instant::now();
    let (page, _, filtered) = store
        .page_query_rows(
            id,
            Some(&last_value),
            PageRequest {
                cursor: None,
                limit: 100,
            },
        )
        .await?;
    let filtered_page_ms = millis(filtered_started.elapsed());
    anyhow::ensure!(
        page.items.len() == 1 && filtered == 1,
        "filtered query mismatch"
    );

    let mut lookups = Vec::with_capacity(args.lookup_samples);
    for _ in 0..args.lookup_samples {
        let started = Instant::now();
        let record = store.get_invocation(id).await?;
        lookups.push(started.elapsed());
        anyhow::ensure!(
            record.request.id == id,
            "point lookup returned wrong record"
        );
    }
    lookups.sort_unstable();
    let store_bytes = directory_size(root.path())?;
    Ok(QueryMetrics {
        stdout_bytes,
        store_bytes,
        postprocess_ms,
        unfiltered_page_ms,
        filtered_page_ms,
        lookup_p50_us: micros(percentile(&lookups, 50)),
        lookup_p95_us: micros(percentile(&lookups, 95)),
    })
}

async fn benchmark_startup(invocations: usize) -> anyhow::Result<StartupMetrics> {
    let root = tempfile::tempdir()?;
    populate_terminal_invocations(root.path(), invocations, 0).await?;
    let store_bytes = directory_size(root.path())?;
    let started = Instant::now();
    let reopened = Store::open(root.path()).await?;
    let reopen_ms = millis(started.elapsed());
    drop(reopened);
    Ok(StartupMetrics {
        retained_invocations: invocations,
        reopen_ms,
        store_bytes,
    })
}

async fn benchmark_gc(invocations: usize) -> anyhow::Result<GcMetrics> {
    let root = tempfile::tempdir()?;
    let store = populate_terminal_invocations(root.path(), invocations, 4 * 1024).await?;
    let bytes_before = directory_size(root.path())?;
    let target_bytes = bytes_before / 2;
    let started = Instant::now();
    let deleted = store
        .enforce_retention(Duration::from_secs(365 * 24 * 60 * 60), target_bytes)
        .await?;
    let elapsed_ms = millis(started.elapsed());
    let bytes_after = directory_size(root.path())?;
    Ok(GcMetrics {
        candidates: invocations,
        bytes_before,
        target_bytes,
        bytes_after,
        deleted,
        elapsed_ms,
    })
}

async fn populate_terminal_invocations(
    root: &Path,
    count: usize,
    evidence_bytes: usize,
) -> anyhow::Result<Store> {
    let store = Store::open(root).await?;
    for ordinal in 0..count {
        let request = InvocationRequest::new(
            PathBuf::from(format!("/benchmark/workspace-{}", ordinal % 16)),
            BazelCommand::Build,
            vec![format!("//benchmark:target-{ordinal}")],
        );
        let id = request.id;
        let paths = store
            .create_invocation(&InvocationRecord::queued(request))
            .await?;
        if evidence_bytes > 0 {
            std::fs::write(paths.stdout, vec![b'x'; evidence_bytes])?;
        }
        store
            .transition(id, InvocationState::Starting, None, None)
            .await?;
        store
            .transition(id, InvocationState::Running, None, None)
            .await?;
        store
            .transition(
                id,
                InvocationState::Succeeded,
                Some(Termination::Exit { code: 0 }),
                Some(InvocationSummary {
                    success: true,
                    headline: "benchmark invocation succeeded".to_owned(),
                    ..InvocationSummary::default()
                }),
            )
            .await?;
    }
    Ok(store)
}

fn write_query_fixture(path: &Path, rows: u64) -> anyhow::Result<()> {
    let mut writer = BufWriter::new(File::create(path)?);
    for ordinal in 0..rows {
        writeln!(writer, "//benchmark/package:target_{ordinal:012}")?;
    }
    writer.flush()?;
    Ok(())
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
