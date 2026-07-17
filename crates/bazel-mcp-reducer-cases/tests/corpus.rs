use std::{env, fs, path::PathBuf};

use bazel_mcp_reducer_cases::{discover_cases, verify_recorded_case};

#[test]
fn all_recorded_reducer_cases_match_their_contracts_and_goldens() {
    let corpus = corpus_root();
    let cases = discover_cases(&corpus).unwrap();
    assert!(
        !cases.is_empty(),
        "the reducer corpus at {} must not be empty (marker={:?}, runfiles={:?}, manifest={:?})",
        corpus.display(),
        env::var_os("REDUCER_CORPUS_MARKER"),
        env::var_os("RUNFILES_DIR").or_else(|| env::var_os("TEST_SRCDIR")),
        env::var_os("RUNFILES_MANIFEST_FILE"),
    );
    for case in cases {
        verify_recorded_case(&case)
            .unwrap_or_else(|error| panic!("case {} failed: {error:#}", case.manifest.id));
    }
}

fn corpus_root() -> PathBuf {
    let marker = env::var_os("REDUCER_CORPUS_MARKER").map(PathBuf::from);
    if let Some(marker) = &marker
        && marker.is_file()
    {
        return marker.parent().unwrap().to_owned();
    }

    for variable in ["RUNFILES_DIR", "TEST_SRCDIR"] {
        if let Some(runfiles) = env::var_os(variable) {
            let runfiles = PathBuf::from(runfiles);
            if let Some(marker) = &marker {
                let candidate = runfiles.join(marker);
                if candidate.is_file() {
                    return candidate.parent().unwrap().to_owned();
                }
            }
            for workspace in [env::var_os("TEST_WORKSPACE"), Some("_main".into())]
                .into_iter()
                .flatten()
            {
                let candidate = runfiles.join(workspace).join("testdata/reducer-corpus");
                if candidate.is_dir() {
                    return candidate;
                }
            }
        }
    }

    if let Some(manifest) = env::var_os("RUNFILES_MANIFEST_FILE") {
        let contents = fs::read_to_string(manifest).expect("read Bazel runfiles manifest");
        if let Some(path) = contents.lines().find_map(|line| {
            let (logical, physical) = line.split_once(' ')?;
            (logical.ends_with("/testdata/reducer-corpus/README.md")
                || marker.as_ref().is_some_and(|marker| {
                    logical.ends_with(&marker.to_string_lossy().replace('\\', "/"))
                }))
            .then(|| PathBuf::from(physical).parent().unwrap().to_owned())
        }) {
            return path;
        }
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/reducer-corpus")
}
