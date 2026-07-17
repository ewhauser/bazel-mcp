use std::{fs, path::Path};

use bazel_mcp_reducer::{TestFailureAccumulator, normalize_terminal_text};
use bazel_mcp_types::DiagnosticLocation;
use serde::Serialize;

const CASES: [&str; 5] = ["ordinary", "subtests", "table", "panic", "repeated"];

#[derive(Serialize)]
struct GoldenFailure<'a> {
    name: &'a str,
    message: &'a str,
    location: &'a Option<DiagnosticLocation>,
}

#[test]
fn go_test_log_goldens_preserve_reviewed_reducer_behavior() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-logs/go");
    for case in CASES {
        assert_golden(&root, case);
    }
}

fn assert_golden(root: &Path, case: &str) {
    let prefix = root.join(case);
    let input = fs::read(prefix.with_extension("log")).unwrap();
    let normalized = normalize_terminal_text(&input);
    let mut accumulator = TestFailureAccumulator::default();
    for line in normalized.lines() {
        accumulator.observe_line(line);
    }
    let failures = accumulator.finish();
    let output = failures
        .iter()
        .map(|failure| GoldenFailure {
            name: &failure.name,
            message: &failure.message,
            location: &failure.location,
        })
        .collect::<Vec<_>>();
    let actual = serde_json::to_string_pretty(&output).unwrap() + "\n";
    let expected_path = prefix.with_extension("expected.json");
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        fs::write(&expected_path, &actual).unwrap();
    } else {
        let expected = fs::read_to_string(&expected_path).unwrap_or_else(|error| {
            panic!(
                "missing reviewed golden {}: {error}; run UPDATE_GOLDENS=1 cargo test -p bazel-mcp-reducer --test test_log_golden",
                expected_path.display()
            )
        });
        assert_eq!(actual, expected, "Go test-log golden changed for {case}");
    }
}
