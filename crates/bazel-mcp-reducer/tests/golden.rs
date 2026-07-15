use std::{fs, path::Path};

use bazel_mcp_bep::{DEFAULT_MAX_FRAME_BYTES, decode_stream_partial};
use bazel_mcp_reducer::{Budget, ReductionInput, reduce_artifacts, reduce_invocation};
use bazel_mcp_types::ArtifactKind;
use serde::Serialize;

const VERSIONS: [u32; 3] = [7, 8, 9];
const CASES: [&str; 5] = [
    "loading",
    "visibility",
    "keep-going-actions",
    "test-outcomes",
    "cached-tests",
];

#[derive(Serialize)]
struct GoldenOutput {
    event_count: usize,
    terminal_error: Option<String>,
    summary: bazel_mcp_types::InvocationSummary,
    artifacts: Vec<bazel_mcp_types::Artifact>,
}

#[test]
fn bazel_major_goldens_preserve_reviewed_reducer_behavior() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for version in VERSIONS {
        for case in CASES {
            assert_golden(&root, version, case);
        }
    }
}

#[test]
fn checked_out_of_order_named_sets_resolve_cycles_and_remote_artifacts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bazel-mcp-bep/tests/fixtures/nested-out-of-order.bep");
    let partial = decode_stream_partial(fs::File::open(path).unwrap(), DEFAULT_MAX_FRAME_BYTES);
    assert!(partial.terminal_error.is_none());
    let artifacts = reduce_artifacts(&partial.events);
    assert_eq!(artifacts.len(), 2);
    assert!(artifacts.iter().any(|artifact| {
        artifact.kind == ArtifactKind::Remote
            && artifact.uri == "bytestream://cache/abc/10"
            && !artifact.locally_available
    }));
    assert!(artifacts.iter().any(|artifact| {
        artifact.kind == ArtifactKind::File
            && artifact.uri == "file://<WORKSPACE>/local.out"
            && artifact.locally_available
    }));
}

fn assert_golden(root: &Path, version: u32, case: &str) {
    let prefix = root.join(format!("bazel-{version}/{case}"));
    let partial = decode_stream_partial(
        fs::File::open(prefix.with_extension("bep")).unwrap(),
        DEFAULT_MAX_FRAME_BYTES,
    );
    let stdout = fs::read(prefix.with_extension("stdout")).unwrap();
    let stderr = fs::read(prefix.with_extension("stderr")).unwrap();
    let exit_code = fs::read_to_string(prefix.with_extension("exit"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let output = GoldenOutput {
        event_count: partial.events.len(),
        terminal_error: partial.terminal_error.as_ref().map(ToString::to_string),
        summary: reduce_invocation(ReductionInput {
            events: &partial.events,
            stdout: &stdout,
            stderr: &stderr,
            exit_code: Some(exit_code),
            elapsed_ms: 0,
            budget: Budget {
                max_items: 100,
                max_bytes: 64 * 1024,
            },
        }),
        artifacts: reduce_artifacts(&partial.events),
    };
    let actual = serde_json::to_string_pretty(&output).unwrap() + "\n";
    let expected_path = prefix.with_extension("expected.json");
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::write(&expected_path, &actual).unwrap();
    } else {
        let expected = fs::read_to_string(&expected_path).unwrap_or_else(|error| {
            panic!(
                "missing reviewed golden {}: {error}; run UPDATE_GOLDENS=1 cargo test -p bazel-mcp-reducer --test golden",
                expected_path.display()
            )
        });
        assert_eq!(
            actual, expected,
            "golden changed for Bazel {version} {case}"
        );
    }
}
