use std::{
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use bazel_mcp_bep::{DEFAULT_MAX_FRAME_BYTES, read_frame};
use bazel_mcp_bes::{
    BesServer,
    codec::BuffaCodec,
    proto::{
        Any, BuildComponentStreamFinished, BuildEvent, OrderedBuildEvent,
        PublishBuildToolEventStreamRequest, PublishBuildToolEventStreamResponseOwnedView, StreamId,
        build_event::Event,
    },
};
use buffa::MessageField;
use clap::Parser;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tonic::{
    Request,
    client::Grpc as ClientGrpc,
    codegen::http::uri::PathAndQuery,
    transport::{Channel, Endpoint},
};

const BUILD_TOOL_STREAM_PATH: &str =
    "/google.devtools.build.v1.PublishBuildEvent/PublishBuildToolEventStream";

#[derive(Debug, Parser)]
#[command(about = "Compare direct BEP-file capture with the loopback Buffa BES transport")]
struct Args {
    /// Complete BEP stream used as the repeatable workload.
    #[arg(long)]
    fixture: Option<PathBuf>,

    /// Number of complete fixture copies in each measured capture.
    #[arg(long, default_value_t = 100)]
    repetitions: usize,

    /// Number of alternating samples retained for each transport.
    #[arg(long, default_value_t = 9)]
    samples: usize,

    /// Label recorded in the JSON result.
    #[arg(long, default_value = "working-tree")]
    label: String,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    label: String,
    fixture: String,
    fixture_events: usize,
    fixture_bytes: usize,
    repetitions: usize,
    events_per_sample: usize,
    bytes_per_sample: usize,
    samples: usize,
    tail: Metrics,
    bes: Metrics,
    bes_over_tail_median_ratio: f64,
    bes_median_delta_ms: f64,
}

#[derive(Debug, Serialize)]
struct Metrics {
    median_ms: f64,
    p95_ms: f64,
    mean_ms: f64,
    median_mib_per_second: f64,
    samples_ms: Vec<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.repetitions > 0, "--repetitions must be positive");
    ensure!(args.samples > 0, "--samples must be positive");
    let fixture = args.fixture.unwrap_or_else(default_fixture);
    let fixture_bytes = std::fs::read(&fixture)
        .with_context(|| format!("read benchmark fixture {}", fixture.display()))?;
    let frames = read_frames(&fixture)?;
    ensure!(!frames.is_empty(), "benchmark fixture has no BEP events");
    let expected = fixture_bytes.repeat(args.repetitions);
    let events_per_sample = frames
        .len()
        .checked_mul(args.repetitions)
        .context("benchmark event count overflow")?;

    let root = tempfile::tempdir().context("create benchmark directory")?;
    let tail_path = root.path().join("tail.bep");
    let bes_path = root.path().join("bes.bep");
    let server = BesServer::start().await?;
    let channel = connect(server.endpoint()).await?;

    // Warm each path once so compilation, connection setup, and filesystem initialization
    // are excluded from the alternating measurements.
    run_tail(&tail_path, &expected).await?;
    run_bes(
        &server,
        channel.clone(),
        &bes_path,
        &frames,
        args.repetitions,
        "warmup",
    )
    .await?;
    verify_output(&tail_path, &expected)?;
    verify_output(&bes_path, &expected)?;

    let mut tail_samples = Vec::with_capacity(args.samples);
    let mut bes_samples = Vec::with_capacity(args.samples);
    for sample in 0..args.samples {
        if sample % 2 == 0 {
            tail_samples.push(run_tail(&tail_path, &expected).await?);
            bes_samples.push(
                run_bes(
                    &server,
                    channel.clone(),
                    &bes_path,
                    &frames,
                    args.repetitions,
                    &format!("sample-{sample}"),
                )
                .await?,
            );
        } else {
            bes_samples.push(
                run_bes(
                    &server,
                    channel.clone(),
                    &bes_path,
                    &frames,
                    args.repetitions,
                    &format!("sample-{sample}"),
                )
                .await?,
            );
            tail_samples.push(run_tail(&tail_path, &expected).await?);
        }
    }
    verify_output(&tail_path, &expected)?;
    verify_output(&bes_path, &expected)?;

    let tail = summarize(&tail_samples, expected.len());
    let bes = summarize(&bes_samples, expected.len());
    let report = Report {
        schema_version: 1,
        label: args.label,
        fixture: fixture.display().to_string(),
        fixture_events: frames.len(),
        fixture_bytes: fixture_bytes.len(),
        repetitions: args.repetitions,
        events_per_sample,
        bytes_per_sample: expected.len(),
        samples: args.samples,
        bes_over_tail_median_ratio: bes.median_ms / tail.median_ms,
        bes_median_delta_ms: bes.median_ms - tail.median_ms,
        tail,
        bes,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn default_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bazel-mcp-reducer/tests/fixtures/bazel-9/test-outcomes.bep")
}

fn read_frames(path: &Path) -> Result<Vec<Vec<u8>>> {
    let mut input = File::open(path)?;
    let mut frames = Vec::new();
    while let Some(frame) = read_frame(&mut input, DEFAULT_MAX_FRAME_BYTES)? {
        frames.push(frame);
    }
    Ok(frames)
}

async fn connect(endpoint: &str) -> Result<Channel> {
    let uri = endpoint.replacen("grpc://", "http://", 1);
    Ok(Endpoint::from_shared(uri)?.connect().await?)
}

async fn run_tail(path: &Path, bytes: &[u8]) -> Result<Duration> {
    let start = Instant::now();
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .await?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(start.elapsed())
}

async fn run_bes(
    server: &BesServer,
    channel: Channel,
    path: &Path,
    frames: &[Vec<u8>],
    repetitions: usize,
    invocation_id: &str,
) -> Result<Duration> {
    let start = Instant::now();
    let capture = server.register(invocation_id, path)?;
    let stream_id = StreamId {
        build_id: format!("build-{invocation_id}"),
        invocation_id: invocation_id.to_owned(),
        component: 3,
    };
    let events = frames.iter().cycle().take(frames.len() * repetitions);
    let requests = events
        .enumerate()
        .map(|(index, frame)| {
            Ok(stream_request(
                stream_id.clone(),
                sequence_number(index)?,
                Event::BazelEvent(Box::new(Any {
                    type_url: "type.googleapis.com/build_event_stream.BuildEvent".to_owned(),
                    value: frame.clone(),
                })),
            ))
        })
        .chain(std::iter::once(Ok(stream_request(
            stream_id.clone(),
            sequence_number(frames.len() * repetitions)?,
            Event::ComponentStreamFinished(Box::new(BuildComponentStreamFinished { r#type: 1 })),
        ))))
        .collect::<Result<Vec<_>>>()?;
    let expected_acknowledgements = requests.len();

    let mut client = ClientGrpc::new(channel);
    client.ready().await?;
    let response = client
        .streaming(
            Request::new(tokio_stream::iter(requests)),
            PathAndQuery::from_static(BUILD_TOOL_STREAM_PATH),
            BuffaCodec::<
                PublishBuildToolEventStreamResponseOwnedView,
                PublishBuildToolEventStreamRequest,
            >::default(),
        )
        .await?;
    let mut acknowledgements = response.into_inner();
    let mut acknowledged = 0;
    while acknowledgements.message().await?.is_some() {
        acknowledged += 1;
    }
    ensure!(
        acknowledged == expected_acknowledgements,
        "BES acknowledged {acknowledged} of {expected_acknowledgements} requests"
    );
    let stats = capture.finish(Duration::from_secs(30)).await?;
    ensure!(
        stats.event_count == frames.len() * repetitions,
        "BES retained {} of {} events",
        stats.event_count,
        frames.len() * repetitions
    );
    Ok(start.elapsed())
}

fn sequence_number(index: usize) -> Result<i64> {
    i64::try_from(index)?
        .checked_add(1)
        .context("BES sequence overflow")
}

fn stream_request(
    stream_id: StreamId,
    sequence_number: i64,
    event: Event,
) -> PublishBuildToolEventStreamRequest {
    PublishBuildToolEventStreamRequest {
        ordered_build_event: MessageField::some(OrderedBuildEvent {
            stream_id: MessageField::some(stream_id),
            sequence_number,
            event: MessageField::some(BuildEvent { event: Some(event) }),
        }),
    }
}

fn verify_output(path: &Path, expected: &[u8]) -> Result<()> {
    let actual = std::fs::read(path)?;
    ensure!(
        actual == expected,
        "{} did not retain byte-identical BEP evidence",
        path.display()
    );
    Ok(())
}

fn summarize(samples: &[Duration], bytes: usize) -> Metrics {
    let mut milliseconds = samples
        .iter()
        .map(|duration| duration.as_secs_f64() * 1_000.0)
        .collect::<Vec<_>>();
    milliseconds.sort_by(f64::total_cmp);
    let median_ms = percentile(&milliseconds, 0.50);
    let mib = bytes as f64 / (1024.0 * 1024.0);
    Metrics {
        median_ms,
        p95_ms: percentile(&milliseconds, 0.95),
        mean_ms: milliseconds.iter().sum::<f64>() / milliseconds.len() as f64,
        median_mib_per_second: mib / (median_ms / 1_000.0),
        samples_ms: milliseconds,
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}
