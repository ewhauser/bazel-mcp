use std::{
    env,
    hint::black_box,
    io::Cursor,
    time::{Duration, Instant},
};

use bazel_mcp_bep::proto::{
    Aborted, ActionExecuted, BuildEvent, BuildEventId, File, NamedSetOfFiles, OptionsParsed,
    OutputGroup, Progress, TargetComplete, TestResult, TestSummary, build_event, build_event_id,
    file,
};
use bazel_mcp_bep::{DEFAULT_MAX_FRAME_BYTES, decode_stream, encode_event_id, encode_frame};
use bazel_mcp_reducer::{
    Budget, ReductionInput, extract_canonical_arguments, reduce_artifacts, reduce_invocation,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

const IMPLEMENTATION: &str = "buffa-owned-view";

#[derive(Clone, Copy, Debug)]
enum Mode {
    Decode,
    Full,
    Hold,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "decode" => Ok(Self::Decode),
            "full" => Ok(Self::Full),
            "hold" => Ok(Self::Hold),
            _ => Err(format!(
                "unknown mode {value:?}; expected decode, full, or hold"
            )),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Full => "full",
            Self::Hold => "hold",
        }
    }
}

#[derive(Debug)]
struct Config {
    mode: Mode,
    events: usize,
    iterations: usize,
    warmup: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Full,
            events: 16_000,
            iterations: 30,
            warmup: 3,
        }
    }
}

#[derive(Serialize)]
struct Report {
    implementation: &'static str,
    mode: &'static str,
    events: usize,
    iterations: usize,
    warmup: usize,
    stream_bytes: usize,
    stream_sha256: String,
    elapsed_seconds: f64,
    checksum: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = parse_args()?;
    let stream = make_stream(config.events);
    let digest = format!("{:x}", Sha256::digest(&stream));

    for _ in 0..config.warmup {
        black_box(run_once(config.mode, &stream)?);
    }

    let started = Instant::now();
    let mut checksum = 0_usize;
    let iterations = if matches!(config.mode, Mode::Hold) {
        1
    } else {
        config.iterations
    };
    for _ in 0..iterations {
        checksum = checksum.wrapping_add(run_once(config.mode, &stream)?);
    }
    let elapsed = started.elapsed();

    let report = Report {
        implementation: IMPLEMENTATION,
        mode: config.mode.as_str(),
        events: config.events,
        iterations,
        warmup: config.warmup,
        stream_bytes: stream.len(),
        stream_sha256: digest,
        elapsed_seconds: duration_seconds(elapsed),
        checksum,
    };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn parse_args() -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--mode" => config.mode = Mode::parse(&value)?,
            "--events" => config.events = parse_positive(&flag, &value)?,
            "--iterations" => config.iterations = parse_positive(&flag, &value)?,
            "--warmup" => {
                config.warmup = value
                    .parse()
                    .map_err(|_| format!("invalid value for {flag}: {value}"))?;
            }
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    Ok(config)
}

fn parse_positive(flag: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid value for {flag}: {value}"))?;
    if parsed == 0 {
        Err(format!("{flag} must be greater than zero"))
    } else {
        Ok(parsed)
    }
}

fn run_once(mode: Mode, stream: &[u8]) -> Result<usize, Box<dyn std::error::Error>> {
    let events = decode_stream(Cursor::new(stream), DEFAULT_MAX_FRAME_BYTES)?;
    if matches!(mode, Mode::Decode | Mode::Hold) {
        let checksum = events.iter().fold(events.len(), |sum, event| {
            sum.wrapping_add(event.view().id.len())
        });
        black_box(&events);
        return Ok(checksum);
    }

    let arguments = extract_canonical_arguments(&events).unwrap_or_default();
    let artifacts = reduce_artifacts(&events);
    let summary = reduce_invocation(ReductionInput {
        events: &events,
        stdout: b"",
        stderr: b"synthetic.cc:7: error: benchmark fallback",
        exit_code: Some(1),
        elapsed_ms: 1,
        budget: Budget::result_default(),
    });
    let checksum = events
        .len()
        .wrapping_add(arguments.len())
        .wrapping_add(artifacts.len())
        .wrapping_add(summary.diagnostics.len())
        .wrapping_add(summary.tests.len())
        .wrapping_add(summary.targets.len());
    black_box((events, arguments, artifacts, summary));
    Ok(checksum)
}

fn make_stream(event_count: usize) -> Vec<u8> {
    let mut stream = Vec::with_capacity(event_count.saturating_mul(256));
    for index in 0..event_count {
        stream.extend_from_slice(&encode_frame(&make_event(index)));
    }
    stream
}

fn make_event(index: usize) -> BuildEvent {
    let group = index / 8;
    let label = format!("//synthetic/package_{:03}:target_{group}", group % 128);
    let set_id = format!("set-{group}");
    let (id, payload) = match index % 8 {
        0 => (
            Vec::new(),
            build_event::Payload::OptionsParsed(Box::new(OptionsParsed {
                startup_options: vec!["--host_jvm_args=-Xmx2g".into()],
                explicit_startup_options: vec!["--max_idle_secs=60".into()],
                cmd_line: vec!["--compilation_mode=fastbuild".into(), label.clone()],
                explicit_cmd_line: vec!["--keep_going".into()],
                invocation_policy: vec![0x08, 0x01],
                tool_tag: "bazel-mcp-buffa-spike".into(),
            })),
        ),
        1 => (
            Vec::new(),
            build_event::Payload::Progress(Box::new(Progress {
                stdout: format!(
                    "[{group}] compiling synthetic target with reusable progress text\n"
                ),
                stderr: String::new(),
            })),
        ),
        2 => (
            encode_id(build_event_id::Id::NamedSet(Box::new(
                build_event_id::NamedSetOfFilesId { id: set_id.clone() },
            ))),
            build_event::Payload::NamedSetOfFiles(Box::new(NamedSetOfFiles {
                files: (0..4)
                    .map(|file_index| {
                        output_file(
                            &format!("artifact-{group}-{file_index}.a"),
                            &format!("file:///tmp/bazel-out/{group}/artifact-{file_index}.a"),
                        )
                    })
                    .collect(),
                file_sets: Vec::new(),
            })),
        ),
        3 => (
            encode_id(build_event_id::Id::TargetCompleted(Box::new(
                build_event_id::TargetCompletedId {
                    label: label.clone(),
                    aspect: String::new(),
                    ..Default::default()
                },
            ))),
            build_event::Payload::Completed(Box::new(TargetComplete {
                success: group.is_multiple_of(3),
                output_group: vec![OutputGroup {
                    name: "default".into(),
                    file_sets: vec![build_event_id::NamedSetOfFilesId { id: set_id }],
                    incomplete: false,
                    inline_files: vec![output_file(
                        &format!("inline-{group}.txt"),
                        &format!("file:///tmp/bazel-out/{group}/inline.txt"),
                    )],
                }],
                tag: vec!["manual".into(), "synthetic".into()],
                important_output: Vec::new(),
                target_kind: "cc_library rule".into(),
                directory_output: Vec::new(),
                failure_detail: vec![0x08, 0x02],
            })),
        ),
        4 => (
            encode_id(build_event_id::Id::ActionCompleted(Box::new(
                build_event_id::ActionCompletedId {
                    primary_output: format!("bazel-out/{group}/failed.o"),
                    label: label.clone(),
                    ..Default::default()
                },
            ))),
            build_event::Payload::Action(Box::new(ActionExecuted {
                success: false,
                exit_code: 1,
                label: label.clone(),
                primary_output: output_file(
                    &format!("failed-{group}.o"),
                    &format!("file:///tmp/bazel-out/{group}/failed.o"),
                )
                .into(),
                r#type: "CppCompile".into(),
                command_line: vec!["clang++".into(), "-c".into(), format!("input-{group}.cc")],
                failure_detail: vec![0x08, 0x03],
                ..Default::default()
            })),
        ),
        5 => (
            encode_id(build_event_id::Id::TestResult(Box::new(
                build_event_id::TestResultId {
                    label: label.clone(),
                    run: 1,
                    shard: 0,
                    attempt: 1,
                    ..Default::default()
                },
            ))),
            build_event::Payload::TestResult(Box::new(TestResult {
                test_action_output: vec![output_file(
                    "test.log",
                    &format!("file:///tmp/bazel-testlogs/{group}/test.log"),
                )],
                test_attempt_duration_millis: 37,
                cached_locally: group.is_multiple_of(2),
                status: 4,
                test_attempt_start_millis_epoch: 1_700_000_000_000,
                warning: vec!["synthetic test warning".into()],
                execution_info: vec![0x0a, 0x01, 0x78],
                status_details: "assertion failed".into(),
            })),
        ),
        6 => (
            encode_id(build_event_id::Id::TestSummary(Box::new(
                build_event_id::TestSummaryId {
                    label,
                    ..Default::default()
                },
            ))),
            build_event::Payload::TestSummary(Box::new(TestSummary {
                total_run_count: 1,
                failed: vec![output_file(
                    "test.log",
                    &format!("file:///tmp/bazel-testlogs/{group}/test.log"),
                )],
                overall_status: 4,
                first_start_time_millis: 1_700_000_000_000,
                last_stop_time_millis: 1_700_000_000_037,
                total_run_duration_millis: 37,
                run_count: 1,
                shard_count: 1,
                attempt_count: 1,
                ..Default::default()
            })),
        ),
        _ => (
            Vec::new(),
            build_event::Payload::Aborted(Box::new(Aborted {
                reason: 6,
                description: format!("synthetic analysis failure for group {group}"),
            })),
        ),
    };
    BuildEvent {
        id,
        children: Vec::new(),
        last_message: index + 1 == usize::MAX,
        payload: Some(payload),
    }
}

fn encode_id(id: build_event_id::Id) -> Vec<u8> {
    encode_event_id(&BuildEventId { id: Some(id) })
}

fn output_file(name: &str, uri: &str) -> File {
    File {
        path_prefix: vec!["bazel-out".into(), "synthetic".into()],
        name: name.into(),
        file: Some(file::File::Uri(uri.into())),
        digest: "0123456789abcdef".into(),
        length: 4_096,
    }
}

fn duration_seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}
