#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

use bazel_mcp_bep::proto::{
    ActionExecuted, BuildEvent, BuildEventId, File, TestResult as BepTestResult,
    TestSummary as BepTestSummary, build_event, build_event_id, file,
};
use bazel_mcp_bep::{encode_event_id, encode_frame};
use bazel_mcp_policy::PolicyConfig;
use bazel_mcp_runner::{
    BepTransport, InspectRequest, InspectView, InvocationProgress, InvocationService, RunnerConfig,
    StarlarkReducerConfig,
};
use bazel_mcp_store::Store;
use bazel_mcp_types::{
    Artifact, ArtifactKind, BazelCommand, DeferredRetrieval, Diagnostic, DiagnosticCategory,
    InspectHint, InspectPayload, InvocationRecord, InvocationRequest, InvocationState,
    InvocationSummary, ResultDisposition, Severity, Termination, TestCase, TestResult, TestStatus,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const TEST_OUTCOMES_BEP: &[u8] =
    include_bytes!("../../bazel-mcp-reducer/tests/fixtures/bazel-9/test-outcomes.bep");

fn failed_test_bep(log_uri: &str, xml_uri: &str) -> Vec<u8> {
    let output = |name: &str, uri: &str| File {
        name: name.to_owned(),
        file: Some(file::File::Uri(uri.to_owned())),
        ..Default::default()
    };
    let result = BuildEvent {
        id: encode_event_id(&BuildEventId {
            id: Some(build_event_id::Id::TestResult(Box::new(
                build_event_id::TestResultId {
                    label: "//pkg:failing".into(),
                    run: 1,
                    attempt: 1,
                    ..Default::default()
                },
            ))),
        }),
        payload: Some(build_event::Payload::TestResult(Box::new(BepTestResult {
            test_action_output: vec![output("test.log", log_uri), output("test.xml", xml_uri)],
            status: 4,
            ..Default::default()
        }))),
        ..Default::default()
    };
    let summary = BuildEvent {
        id: encode_event_id(&BuildEventId {
            id: Some(build_event_id::Id::TestSummary(Box::new(
                build_event_id::TestSummaryId {
                    label: "//pkg:failing".into(),
                    ..Default::default()
                },
            ))),
        }),
        payload: Some(build_event::Payload::TestSummary(Box::new(
            BepTestSummary {
                failed: vec![output("test.log", log_uri)],
                overall_status: 4,
                total_run_duration_millis: 12,
                attempt_count: 1,
                ..Default::default()
            },
        ))),
        ..Default::default()
    };
    [encode_frame(&result), encode_frame(&summary)].concat()
}

async fn wait_for_path(path: &Path) {
    for _ in 0..500 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {}", path.display());
}

async fn wait_for_invocation(service: &InvocationService, id: bazel_mcp_types::InvocationId) {
    for _ in 0..500 {
        if service.test_support().get_invocation(id).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for invocation {id}");
}

async fn wait_for_output_base_wait(
    service: &InvocationService,
    id: bazel_mcp_types::InvocationId,
) -> InvocationProgress {
    for _ in 0..500 {
        if let Ok(progress) = service.invocation_progress(id).await
            && progress.phase == Some("output_base_lock_wait")
            && progress.output_base_lock_wait_ms > 0
        {
            return progress;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for output-base lock progress for {id}");
}

async fn service(root: &TempDir, workspace: &Path, script: &str) -> InvocationService {
    service_with_redaction(root, workspace, script, Vec::new()).await
}

async fn service_with_redaction(
    root: &TempDir,
    workspace: &Path,
    script: &str,
    redaction_patterns: Vec<String>,
) -> InvocationService {
    tokio::fs::write(workspace.join("MODULE.bazel"), "module(name='test')\n")
        .await
        .unwrap();
    let executable = root.path().join("fake-bazel");
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then echo 'bazel 9.1.0'; exit 0; fi\n{}",
        script.strip_prefix("#!/bin/sh\n").unwrap_or(script)
    );
    tokio::fs::write(&executable, script).await.unwrap();
    tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let store = Store::open(root.path().join("store")).await.unwrap();
    let policy = PolicyConfig {
        allowed_roots: vec![root.path().to_owned()],
        bazel_executable: Some(executable),
        redaction_patterns,
        ..PolicyConfig::default()
    };
    InvocationService::new(
        store,
        RunnerConfig {
            policy,
            cancellation_interrupt_grace: Duration::from_millis(100),
            cancellation_terminate_grace: Duration::from_millis(100),
            ..RunnerConfig::default()
        },
    )
    .unwrap()
}

async fn configured_service(
    root: &TempDir,
    workspace: &Path,
    script: &str,
    configure: impl FnOnce(&mut RunnerConfig),
) -> InvocationService {
    tokio::fs::write(workspace.join("MODULE.bazel"), "module(name='test')\n")
        .await
        .unwrap();
    let executable = root.path().join("configured-fake-bazel");
    tokio::fs::write(&executable, script).await.unwrap();
    tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let policy = PolicyConfig {
        allowed_roots: vec![root.path().to_owned()],
        bazel_executable: Some(executable),
        ..PolicyConfig::default()
    };
    let mut config = RunnerConfig {
        policy,
        cancellation_interrupt_grace: Duration::from_millis(100),
        cancellation_terminate_grace: Duration::from_millis(100),
        ..RunnerConfig::default()
    };
    configure(&mut config);
    InvocationService::start(
        Store::open(root.path().join("configured-store"))
            .await
            .unwrap(),
        config,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn routes_configured_lint_through_aspect_and_keeps_query_on_bazel() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let aspect = root.path().join("fake-aspect");
    let aspect_log = root.path().join("aspect-args.log");
    let bazel_log = root.path().join("bazel-args.log");
    let aspect_script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$BAZEL_REAL\" \"$@\" > '{}'\necho '🚨 src/app.ts:8:3 · eslint · no-console — unexpected console statement' >&2\nexit 1\n",
        aspect_log.display()
    );
    tokio::fs::write(&aspect, aspect_script).await.unwrap();
    tokio::fs::set_permissions(&aspect, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let bazel_script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nprintf '%s\\n' \"$@\" > '{}'\necho '//pkg:target'\n",
        bazel_log.display()
    );
    let configured_bazel = root.path().join("configured-fake-bazel");
    let output_user_root = root.path().join("output-user-root");
    let service = configured_service(&root, &workspace, &bazel_script, |config| {
        config.policy.allowed_commands.insert("lint".to_owned());
        config.aspect.executable = Some(aspect.clone());
        config.aspect.commands.insert("lint".to_owned());
        config.output_user_root = Some(output_user_root.clone());
    })
    .await;

    let mut lint_request = InvocationRequest::new(
        workspace.clone(),
        BazelCommand::Custom("lint".to_owned()),
        vec!["//...".to_owned(), "--config=ci".to_owned()],
    );
    lint_request.startup_arguments = vec!["--host_jvm_args=-Xmx1g".to_owned()];
    let lint_id = lint_request.id;
    let lint = service.run(lint_request).await.unwrap();

    assert_eq!(lint.state, InvocationState::Failed);
    let summary = lint.summary.unwrap();
    assert_eq!(summary.diagnostics.len(), 1);
    assert_eq!(
        summary.diagnostics[0].message,
        "eslint [no-console]: unexpected console statement"
    );
    assert!(summary.headline.starts_with("Aspect lint failed:"));
    let aspect_args = tokio::fs::read_to_string(&aspect_log).await.unwrap();
    let aspect_args = aspect_args.lines().collect::<Vec<_>>();
    assert_eq!(aspect_args[0], configured_bazel.to_string_lossy());
    assert!(aspect_args.contains(&format!("--task:id={lint_id}").as_str()));
    assert!(aspect_args.contains(&"--task:timing-summary=none"));
    assert!(aspect_args.contains(&"lint"));
    assert!(
        aspect_args.contains(
            &format!(
                "--bazel-startup-flag=--output_user_root={}",
                output_user_root.display()
            )
            .as_str()
        )
    );
    assert!(aspect_args.contains(&"--bazel-startup-flag=--host_jvm_args=-Xmx1g"));
    assert!(aspect_args.contains(&format!("--bazel-flag=--invocation_id={lint_id}").as_str()));
    assert!(
        aspect_args
            .iter()
            .any(|argument| argument.starts_with("--bes-backend=grpc://"))
    );
    assert!(
        aspect_args.contains(&format!("--bes-header=x-bazel-mcp-invocation-id={lint_id}").as_str())
    );
    assert!(aspect_args.contains(&"//..."));
    assert!(aspect_args.contains(&"--config=ci"));
    assert!(
        !aspect_args
            .iter()
            .any(|argument| argument.starts_with("--build_event_binary_file"))
    );

    let query = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Query,
            vec!["//...".to_owned()],
        ))
        .await
        .unwrap();
    assert_eq!(query.state, InvocationState::Succeeded);
    let bazel_args = tokio::fs::read_to_string(bazel_log).await.unwrap();
    assert!(bazel_args.lines().any(|argument| argument == "query"));
    assert!(!bazel_args.lines().any(|argument| argument == "lint"));
}

async fn shared_executable_service(
    root: &TempDir,
    workspace: &Path,
    executable: &Path,
    store_name: &str,
    output_base_lock_root: &Path,
) -> InvocationService {
    tokio::fs::write(workspace.join("MODULE.bazel"), "module(name='test')\n")
        .await
        .unwrap();
    let policy = PolicyConfig {
        allowed_roots: vec![root.path().to_owned()],
        bazel_executable: Some(executable.to_owned()),
        ..PolicyConfig::default()
    };
    InvocationService::new(
        Store::open(root.path().join(store_name)).await.unwrap(),
        RunnerConfig {
            policy,
            cancellation_interrupt_grace: Duration::from_millis(100),
            cancellation_terminate_grace: Duration::from_millis(100),
            output_base_lock_root: output_base_lock_root.to_owned(),
            ..RunnerConfig::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn configured_starlark_reducer_augments_redacted_native_evidence() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let reducer = root.path().join("custom.star");
    tokio::fs::write(
        &reducer,
        r#"
API_VERSION = 1
NAME = "custom-compiler"
COMMANDS = ["build"]

def reduce(ctx):
    diagnostics = regex_diagnostics(
        ctx["stderr"],
        r"error: (?P<message>.+)",
        category = "compilation",
    )
    return patch(diagnostics, headline = "Custom compiler failed")
"#,
    )
    .await
    .unwrap();
    let service = configured_service(
        &root,
        &workspace,
        "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\necho 'error: token=SUPERSECRET custom root cause' >&2\nexit 1\n",
        |config| {
            config.policy.redaction_patterns = vec![r"token=[^\s]+".to_owned()];
            config.starlark_reducers = StarlarkReducerConfig {
                files: vec![reducer],
                ..StarlarkReducerConfig::default()
            };
        },
    )
    .await;

    let record = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//custom:target".to_owned()],
        ))
        .await
        .unwrap();

    let summary = record.summary.unwrap();
    let encoded = serde_json::to_string(&summary).unwrap();
    assert!(!encoded.contains("SUPERSECRET"));
    assert_eq!(summary.headline, "Custom compiler failed", "{encoded}");
    assert!(
        summary
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("[REDACTED] custom root cause"))
    );
}

#[tokio::test]
async fn redacts_starlark_load_failures_before_returning_configuration_errors() {
    let root = tempfile::tempdir().unwrap();
    let reducer = root.path().join("invalid.star");
    tokio::fs::write(&reducer, "fail(\"token=SUPERSECRET\")\n")
        .await
        .unwrap();
    let store = Store::open(root.path().join("load-error-store"))
        .await
        .unwrap();
    let error = InvocationService::new(
        store,
        RunnerConfig {
            policy: PolicyConfig {
                redaction_patterns: vec![r"token=[^\s]+".to_owned()],
                ..PolicyConfig::default()
            },
            starlark_reducers: StarlarkReducerConfig {
                files: vec![reducer],
                ..StarlarkReducerConfig::default()
            },
            ..RunnerConfig::default()
        },
    )
    .err()
    .expect("invalid Starlark reducer must prevent startup")
    .to_string();

    assert!(!error.contains("SUPERSECRET"), "{error}");
    assert!(error.contains("[REDACTED]"), "{error}");
}

#[tokio::test]
async fn keeps_a_bounded_custom_reducer_failure_notice_when_native_diagnostics_are_full() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let reducer = root.path().join("runtime-failure.star");
    tokio::fs::write(
        &reducer,
        "API_VERSION = 1\nNAME = \"runtime-failure\"\ndef reduce(ctx): return 1 // 0\n",
    )
    .await
    .unwrap();
    let errors = (0..30)
        .map(|index| format!("echo 'file{index}.cc:1: error: failure {index}' >&2\n"))
        .collect::<String>();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n{errors}exit 1\n"
    );
    let service = configured_service(&root, &workspace, &script, |config| {
        config.starlark_reducers = StarlarkReducerConfig {
            files: vec![reducer],
            ..StarlarkReducerConfig::default()
        };
    })
    .await;

    let record = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//custom:target".to_owned()],
        ))
        .await
        .unwrap();
    let summary = record.summary.unwrap();

    assert!(summary.truncated);
    assert!(summary.diagnostics.len() <= 20);
    assert!(summary.diagnostics.iter().any(|diagnostic| {
        diagnostic.severity == Severity::Note
            && diagnostic
                .message
                .contains("native reducer output was retained")
    }));
}

#[tokio::test]
async fn redacts_secrets_from_metadata_normalized_rows_and_log_inspection() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let secret = "token=SUPERSECRET";
    let service = service_with_redaction(
        &root,
        &workspace,
        "#!/bin/sh\necho 'file.cc:1: error: token=SUPERSECRET ROOT_CAUSE' >&2\nexit 1\n",
        vec![r"token=[^\s]+".to_owned()],
    )
    .await;
    let request = InvocationRequest::new(
        workspace,
        BazelCommand::Build,
        vec!["//...".into(), secret.into()],
    );
    let id = request.id;
    let record = service.run(request).await.unwrap();
    assert_eq!(record.state, InvocationState::Failed);
    assert!(!serde_json::to_string(&record).unwrap().contains(secret));

    for view in [InspectView::Diagnostics, InspectView::Log] {
        let inspected = service
            .inspect(InspectRequest {
                invocation_id: Some(id),
                workspace: None,
                state: None,
                command: None,
                view,
                cursor: None,
                filter: None,
                item_limit: 20,
                scan_limit: 10_000,
            })
            .await
            .unwrap();
        assert!(!serde_json::to_string(&inspected).unwrap().contains(secret));
    }
    let secret_filter = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: Some("SUPERSECRET".into()),
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert!(secret_filter.items.is_empty());

    service
        .test_support()
        .replace_artifacts(
            id,
            &[Artifact {
                name: "remote-coverage.dat".into(),
                kind: ArtifactKind::Coverage,
                uri: "bytestream://example/coverage".into(),
                size_bytes: Some(100),
                locally_available: false,
            }],
        )
        .await
        .unwrap();
    let coverage = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Coverage,
            cursor: None,
            filter: None,
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&coverage)
            .unwrap()
            .contains("remote_artifact_unavailable")
    );
}

#[tokio::test]
async fn preserves_raw_bep_before_redacting_every_persisted_and_visible_projection() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let secret = "token=PIPELINESECRET";
    let raw_bep = encode_frame(&BuildEvent {
        payload: Some(build_event::Payload::Action(Box::new(ActionExecuted {
            success: false,
            exit_code: 1,
            r#type: secret.to_owned(),
            ..Default::default()
        }))),
        ..Default::default()
    });
    let fixture = root.path().join("secret.bep");
    tokio::fs::write(&fixture, &raw_bep).await.unwrap();
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do case \"$arg\" in --build_event_binary_file=*) bep_path=${{arg#*=}} ;; esac; done\ncp '{}' \"$bep_path\"\nexit 1\n",
        fixture.display()
    );
    let service =
        service_with_redaction(&root, &workspace, &script, vec![r"token=[^\s]+".to_owned()]).await;
    let request = InvocationRequest::new(workspace, BazelCommand::Build, vec!["//:secret".into()]);
    let id = request.id;
    let record = service.run(request).await.unwrap();
    assert_eq!(record.state, InvocationState::Failed);
    assert!(!serde_json::to_string(&record).unwrap().contains(secret));

    let paths = service.test_support().paths_for(&record);
    let retained = std::fs::read(&paths.bep).unwrap();
    assert_eq!(retained, raw_bep);
    assert!(
        retained
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes())
    );
    for path in [&paths.manifest, &paths.details, &paths.artifacts] {
        if path.exists() {
            assert!(
                !String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains(secret),
                "{} retained an unredacted projection",
                path.display()
            );
        }
    }

    let inspected = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Diagnostics,
            cursor: None,
            filter: None,
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert!(!serde_json::to_string(&inspected).unwrap().contains(secret));
}

#[tokio::test]
async fn starlark_source_locations_are_workspace_relative() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\necho \"ERROR: $PWD/pkg/defs.bzl:4:2: name 'missing_rule' is not defined\" >&2\nexit 1\n",
    )
    .await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//pkg:broken".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert!(summary.headline.contains("missing_rule"));
    let diagnostic = &summary.diagnostics[0];
    assert_eq!(diagnostic.category, DiagnosticCategory::Loading);
    assert_eq!(diagnostic.message, "name 'missing_rule' is not defined");
    assert_eq!(diagnostic.location.as_ref().unwrap().path, "pkg/defs.bzl");
    assert_eq!(diagnostic.location.as_ref().unwrap().line, Some(4));
    assert_eq!(diagnostic.location.as_ref().unwrap().column, Some(2));
}

#[tokio::test]
async fn protobuf_source_locations_are_workspace_relative() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\necho \"$PWD/proto/schema.proto:6:3: \\\"MissingMessage\\\" is not defined.\" >&2\nexit 1\n",
    )
    .await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//proto:schema".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert!(summary.headline.contains("MissingMessage"));
    let diagnostic = &summary.diagnostics[0];
    assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
    assert_eq!(diagnostic.message, "\"MissingMessage\" is not defined.");
    assert_eq!(
        diagnostic.location.as_ref().unwrap().path,
        "proto/schema.proto"
    );
    assert_eq!(diagnostic.location.as_ref().unwrap().line, Some(6));
    assert_eq!(diagnostic.location.as_ref().unwrap().column, Some(3));
}

#[tokio::test]
async fn go_compiler_run_and_diagnostics_inspection_omit_bazel_status_lines() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\necho 'INFO: Analyzed target //example:broken (8 packages loaded, 12 targets configured)' >&2\necho 'main.go:3:17: cannot use \"not an integer\" (untyped string constant) as int value in variable declaration' >&2\necho 'ERROR: Build did NOT complete successfully' >&2\nexit 1\n",
    )
    .await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//example:broken".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert_eq!(summary.diagnostics.len(), 1);
    let diagnostic = &summary.diagnostics[0];
    assert_eq!(diagnostic.category, DiagnosticCategory::Compilation);
    assert_eq!(
        diagnostic.message,
        "cannot use \"not an integer\" (untyped string constant) as int value in variable declaration"
    );
    assert_eq!(
        diagnostic.location,
        Some(bazel_mcp_types::DiagnosticLocation {
            path: "main.go".into(),
            line: Some(3),
            column: Some(17),
        })
    );
    assert!(summary.diagnostics.iter().all(|diagnostic| {
        !diagnostic.message.starts_with("INFO:")
            && !diagnostic
                .message
                .contains("Build did NOT complete successfully")
    }));

    let inspected = service
        .inspect(InspectRequest {
            invocation_id: Some(failed.request.id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Diagnostics,
            cursor: None,
            filter: None,
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert_eq!(
        inspected.items,
        InspectPayload::Diagnostics(vec![diagnostic.clone()])
    );
}

#[tokio::test]
async fn log_inspection_uses_bounded_opaque_cursors() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 200 ]; do\n  printf 'line-%03d-abcdefghijklmnopqrstuvwxyz-abcdefghijklmnopqrstuvwxyz\\n' \"$i\" >&2\n  i=$((i+1))\ndone\n",
    )
    .await;
    let request =
        InvocationRequest::new(workspace.clone(), BazelCommand::Build, vec!["//...".into()]);
    let id = request.id;
    service.run(request).await.unwrap();

    let first = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: None,
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert!(first.truncated);
    let cursor = first.next_cursor.unwrap();
    assert!(cursor.parse::<u64>().is_err());

    let second = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: Some(cursor.clone()),
            filter: None,
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert_ne!(first.items, second.items);

    let other_request =
        InvocationRequest::new(workspace.clone(), BazelCommand::Build, vec!["//...".into()]);
    let other_id = other_request.id;
    service.run(other_request).await.unwrap();
    assert!(
        service
            .inspect(InspectRequest {
                invocation_id: Some(other_id),
                workspace: None,
                state: None,
                command: None,
                view: InspectView::Log,
                cursor: Some(cursor),
                filter: None,
                item_limit: 20,
                scan_limit: 10_000,
            })
            .await
            .is_err()
    );

    let invalid = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: Some("512".into()),
            filter: None,
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await;
    assert!(invalid.is_err());

    let filtered = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: Some("LINE-150".into()),
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    let InspectPayload::Log(filtered_items) = filtered.items else {
        panic!("log inspection returned a different payload type");
    };
    assert_eq!(filtered_items.len(), 1);
    assert!(filtered_items[0].contains("line-150"));

    let unmatched = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: Some("not-present".into()),
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert!(unmatched.items.is_empty());
}

#[tokio::test]
async fn failed_test_logs_are_snapshotted_and_retrieved_without_a_public_uri() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        "setup\npkg/failing_test.go:42: got 1, want 2\ncaused by adjacent detail token=SUPERSECRET\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="failing_case" time="0.01"><failure message="expected true"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-test.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let flag_marker = root.path().join("saw-test-output-errors");
    let failed_once = root.path().join("failed-once");
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n    --test_output=errors) touch '{}' ;;\n  esac\ndone\nif [ -f '{}' ]; then : > \"$bep_path\"; exit 0; fi\ncp '{}' \"$bep_path\"\ntouch '{}'\necho 'test execution failed'\nexit 1\n",
        flag_marker.display(),
        failed_once.display(),
        bep.display(),
        failed_once.display(),
    );
    let service = configured_service(&root, &workspace, &script, |config| {
        config.policy.redaction_patterns = vec![r"token=[^\s]+".to_owned()];
    })
    .await;

    let failed = service
        .run(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();
    assert_eq!(failed.state, InvocationState::Failed);
    assert!(flag_marker.exists());
    let summary = failed.summary.as_ref().unwrap();
    assert!(summary.headline.contains("got 1, want 2"));
    assert_eq!(summary.inspect_hint, Some(InspectHint::TestLog));
    let diagnostic = summary
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("got 1, want 2"))
        .unwrap();
    assert_eq!(
        summary
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("got 1, want 2"))
            .count(),
        1
    );
    assert_eq!(diagnostic.category, DiagnosticCategory::Test);
    assert_eq!(
        diagnostic.location.as_ref().unwrap().path,
        "pkg/failing_test.go"
    );
    assert_eq!(diagnostic.location.as_ref().unwrap().line, Some(42));
    assert!(summary.tests[0].test_log_available);
    assert_eq!(summary.tests[0].cases[0].name, "failing_case");
    assert!(!serde_json::to_string(summary).unwrap().contains("log_uri"));

    let inspected = service
        .inspect(InspectRequest {
            invocation_id: Some(failed.request.id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::TestLog,
            cursor: None,
            filter: Some("//PKG:FAILING".into()),
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    let InspectPayload::TestLog(items) = inspected.items else {
        panic!("test-log inspection returned a different payload type");
    };
    assert!(items.iter().any(|item| item.contains("got 1, want 2")));

    let contextual = service
        .inspect(InspectRequest {
            invocation_id: Some(failed.request.id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::TestLog,
            cursor: None,
            filter: Some("got 1, want 2".into()),
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert_eq!(
        contextual.items,
        InspectPayload::TestLog(vec![
            "[//pkg:failing] setup".to_owned(),
            "[//pkg:failing] pkg/failing_test.go:42: got 1, want 2".to_owned(),
            "[//pkg:failing] caused by adjacent detail [REDACTED]".to_owned(),
        ])
    );

    let failed_paths = service.test_support().paths_for(&failed);
    let retained_before = tokio::fs::read(&failed_paths.test_logs_raw).await.unwrap();
    assert!(String::from_utf8_lossy(&retained_before).contains("got 1, want 2"));
    tokio::fs::remove_file(&flag_marker).await.unwrap();
    let passing = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Coverage,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();
    assert_eq!(passing.state, InvocationState::Succeeded);
    assert!(flag_marker.exists());
    assert_eq!(
        tokio::fs::read(&failed_paths.test_logs_raw).await.unwrap(),
        retained_before
    );
}

#[tokio::test]
async fn failed_python_test_logs_promote_the_located_traceback_cause() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        r#"Traceback (most recent call last):
  File "/tmp/output/failing.runfiles/_main/pkg/failing_test.py", line 7, in test_total
    self.assertEqual(actual_total, 41)
AssertionError: 42 != 41 : invoice total should include the fee
FAILED (failures=1)
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="test_total"><failure message="invoice total mismatch"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-python-test.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n  esac\ndone\ncp '{}' \"$bep_path\"\necho 'AssertionError: 42 != 41 : invoice total should include the fee' >&2\nexit 1\n",
        bep.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert_eq!(summary.inspect_hint, Some(InspectHint::TestLog));
    assert!(
        summary
            .headline
            .contains("invoice total should include the fee")
    );
    let matching = summary
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("invoice total should include the fee")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].category, DiagnosticCategory::Test);
    assert_eq!(
        matching[0].location.as_ref().unwrap().path,
        "pkg/failing_test.py"
    );
    assert_eq!(matching[0].location.as_ref().unwrap().line, Some(7));
}

#[tokio::test]
async fn failed_java_test_logs_promote_the_located_application_frame() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        r#"java.lang.AssertionError: invoice total should include the fee
    at org.junit.Assert.fail(Assert.java:89)
    at mcp.java_fixture.RuntimeFailure.assertInvoiceTotal(RuntimeFailure.java:9)
    at mcp.java_fixture.RuntimeFailure.main(RuntimeFailure.java:5)
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="assertInvoiceTotal"><failure message="invoice total mismatch"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-java-test.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n  esac\ndone\ncp '{}' \"$bep_path\"\nprintf '%s\\n' 'java.lang.AssertionError: invoice total should include the fee' '    at mcp.java_fixture.RuntimeFailure.assertInvoiceTotal(RuntimeFailure.java:9)' >&2\nexit 1\n",
        bep.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert_eq!(summary.inspect_hint, Some(InspectHint::TestLog));
    let matching = summary
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("invoice total should include the fee")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].category, DiagnosticCategory::Test);
    assert_eq!(
        matching[0].location.as_ref().unwrap().path,
        "mcp/java_fixture/RuntimeFailure.java"
    );
    assert_eq!(matching[0].location.as_ref().unwrap().line, Some(9));
}

#[tokio::test]
async fn failed_javascript_test_logs_promote_the_located_application_frame() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        r#"/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:2
return invoice.lines.reduce((total, line) => total + line.amount, 0);
               ^

TypeError: Cannot read properties of undefined (reading 'lines')
    at calculateInvoiceTotal (/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:2:18)
    at Object.<anonymous> (/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:6:1)
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="calculateInvoiceTotal"><failure message="invoice was undefined"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-javascript-test.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n  esac\ndone\ncp '{}' \"$bep_path\"\nprintf '%s\\n' \"TypeError: Cannot read properties of undefined (reading 'lines')\" '    at calculateInvoiceTotal (/tmp/output/runtime_type_error.runfiles/_main/mcp/js_fixture/runtime_type_error.js:2:18)' >&2\nexit 1\n",
        bep.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert_eq!(summary.inspect_hint, Some(InspectHint::TestLog));
    let matching = summary
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("undefined (reading 'lines')"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].category, DiagnosticCategory::Test);
    assert_eq!(matching[0].target.as_deref(), Some("//pkg:failing"));
    assert_eq!(
        matching[0].location.as_ref().unwrap().path,
        "mcp/js_fixture/runtime_type_error.js"
    );
    assert_eq!(matching[0].location.as_ref().unwrap().line, Some(2));
    assert_eq!(matching[0].location.as_ref().unwrap().column, Some(18));
}

#[tokio::test]
async fn failed_gtest_logs_promote_the_located_assertion_block() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        r#"[ RUN      ] InvoiceTest.IncludesServiceFee
mcp/cpp_fixture/assertion_failure_test.cc:8: Failure
Expected equality of these values:
CalculateInvoiceTotal(40, 2)
Which is: 42
41
invoice total should include the service fee
[  FAILED  ] InvoiceTest.IncludesServiceFee (0 ms)
"#,
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="IncludesServiceFee"><failure message="exited with error code 1"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-gtest.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n  esac\ndone\ncp '{}' \"$bep_path\"\necho 'test execution failed' >&2\nexit 1\n",
        bep.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();
    let summary = failed.summary.as_ref().unwrap();
    assert_eq!(summary.inspect_hint, Some(InspectHint::TestLog));
    let matching = summary
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("invoice total should include the service fee")
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].category, DiagnosticCategory::Test);
    assert_eq!(matching[0].target.as_deref(), Some("//pkg:failing"));
    assert_eq!(
        matching[0].location.as_ref().unwrap().path,
        "mcp/cpp_fixture/assertion_failure_test.cc"
    );
    assert_eq!(matching[0].location.as_ref().unwrap().line, Some(8));
    assert!(summary.headline.contains("InvoiceTest.IncludesServiceFee"));
}

#[tokio::test]
async fn failed_go_test_logs_promote_the_located_subtest_assertion() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        "=== RUN   TestInvoiceTotal\n\
=== RUN   TestInvoiceTotal/service_fee\n\
    invoice_test.go:18: got 42; want 41\n\
--- FAIL: TestInvoiceTotal (0.00s)\n\
    --- FAIL: TestInvoiceTotal/service_fee (0.00s)\n\
FAIL\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="go_default_test"><failure message="exited with error code 1"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-go-test.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n  esac\ndone\ncp '{}' \"$bep_path\"\necho 'invoice_test.go:18: got 42; want 41'\nexit 1\n",
        bep.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();

    let summary = failed.summary.as_ref().unwrap();
    assert_eq!(summary.inspect_hint, Some(InspectHint::TestLog));
    assert!(summary.headline.contains("TestInvoiceTotal/service_fee"));
    assert!(summary.headline.contains("got 42; want 41"));
    let matching = summary
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("got 42; want 41"))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1);
    let diagnostic = matching[0];
    assert_eq!(diagnostic.category, DiagnosticCategory::Test);
    assert_eq!(diagnostic.target.as_deref(), Some("//pkg:failing"));
    assert_eq!(
        diagnostic.location.as_ref().unwrap().path,
        "invoice_test.go"
    );
    assert_eq!(diagnostic.location.as_ref().unwrap().line, Some(18));
    assert!(
        summary
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("=== RUN")
                && diagnostic.message != "FAIL")
    );
    assert_eq!(
        summary.tests[0].cases[0].name,
        "TestInvoiceTotal/service_fee"
    );
}

#[tokio::test]
async fn failed_rust_test_log_promotes_case_and_assertion_to_initial_summary() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_root = root.path().join("output-user-root");
    let test_directory = output_root.join("execroot/ws/bazel-out/testlogs/pkg/failing");
    tokio::fs::create_dir_all(&workspace).await.unwrap();
    tokio::fs::create_dir_all(&test_directory).await.unwrap();
    let test_log = test_directory.join("test.log");
    let test_xml = test_directory.join("test.xml");
    tokio::fs::write(
        &test_log,
        "running 3 tests\n\
test build::tests::successful_root_cause_test ... ok\n\
test test::tests::parses_direct_testsuite_root ... FAILED\n\
test test::tests::another_success ... ok\n\
\n\
failures:\n\
\n\
---- test::tests::parses_direct_testsuite_root stdout ----\n\
thread 'test::tests::parses_direct_testsuite_root' (3670855) panicked at crates/bazel-mcp-reducer/src/test.rs:101:9:\n\
assertion `left == right` failed\n\
left: \"one\"\n\
right: \"expected\"\n\
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\
\n\
failures:\n\
    test::tests::parses_direct_testsuite_root\n\
\n\
test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
    )
    .await
    .unwrap();
    tokio::fs::write(
        &test_xml,
        r#"<testsuites><testsuite><testcase name="crates/bazel-mcp-reducer/unit_test" time="0.01"><failure message="exited with error code 101"/></testcase></testsuite></testsuites>"#,
    )
    .await
    .unwrap();
    let bep = root.path().join("failed-rust-test.bep");
    tokio::fs::write(
        &bep,
        failed_test_bep(
            &format!("file://{}", test_log.display()),
            &format!("file://{}", test_xml.display()),
        ),
    )
    .await
    .unwrap();
    let flag_marker = root.path().join("saw-test-output-errors");
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n    --test_output=errors) touch '{}' ;;\n  esac\ndone\ncp '{}' \"$bep_path\"\nexit 1\n",
        flag_marker.display(),
        bep.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

    let failed = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Test,
            vec!["//pkg:failing".into()],
        ))
        .await
        .unwrap();

    assert_eq!(failed.state, InvocationState::Failed);
    assert!(flag_marker.exists());
    let summary = failed.summary.as_ref().unwrap();
    assert!(
        summary
            .headline
            .contains("test::tests::parses_direct_testsuite_root")
    );
    assert!(
        summary
            .headline
            .contains("assertion `left == right` failed")
    );
    assert_eq!(summary.diagnostics[0].category, DiagnosticCategory::Test);
    assert_eq!(
        summary.diagnostics[0].location.as_ref().unwrap().path,
        "crates/bazel-mcp-reducer/src/test.rs"
    );
    assert!(
        summary
            .diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.ends_with(" ... ok"))
    );
    assert!(summary.tests[0].test_log_available);
    assert_eq!(
        summary.tests[0].cases[0].name,
        "test::tests::parses_direct_testsuite_root"
    );
    assert!(
        summary.tests[0].cases[0]
            .message
            .as_ref()
            .unwrap()
            .contains("right: \"expected\"")
    );
}

#[tokio::test]
async fn cancellation_stops_a_running_process_group() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\ntrap 'exit 130' INT TERM\nsleep 30\nexit 0\n",
    )
    .await;
    let request = InvocationRequest::new(workspace, BazelCommand::Build, vec!["//...".into()]);
    let id = request.id;
    let running = tokio::spawn({
        let service = service.clone();
        async move { service.run(request).await.unwrap() }
    });
    wait_for_invocation(&service, id).await;
    assert!(service.cancel(id).await.unwrap().cancellation_requested);
    let record = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, InvocationState::Cancelled);
}

#[tokio::test]
async fn deferred_submission_commits_before_completion_and_survives_wait_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let launches = root.path().join("launches");
    let service = service(
        &root,
        &workspace,
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = //:warm ] && exit 0; done\necho run >> '{}'\nsleep 0.4\nexit 0\n",
            launches.display()
        ),
    )
    .await;
    service
        .run(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Build,
            vec!["//:warm".into()],
        ))
        .await
        .unwrap();
    let request = InvocationRequest::new(
        workspace.clone(),
        BazelCommand::Build,
        vec!["//:deferred".into()],
    );
    let id = request.id;
    let started = Instant::now();
    let accepted = service
        .submit(
            request,
            ResultDisposition::Deferred {
                retrieval: DeferredRetrieval::InlineResult,
                expires_at_ms: bazel_mcp_types::unix_timestamp_ms() + 60_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(accepted, id);
    assert!(started.elapsed() < Duration::from_millis(300));
    let visible = service
        .deferred_result(id, DeferredRetrieval::InlineResult)
        .await
        .unwrap();
    assert!(!visible.invocation.state.is_terminal());

    let cancelled_wait = CancellationToken::new();
    cancelled_wait.cancel();
    assert!(matches!(
        service.wait(id, cancelled_wait).await,
        Err(bazel_mcp_runner::RunnerError::WaitCancelled(found)) if found == id
    ));

    let waiters = (0..64)
        .map(|_| {
            let service = service.clone();
            tokio::spawn(async move { service.wait(id, CancellationToken::new()).await })
        })
        .collect::<Vec<_>>();
    for waiter in waiters {
        let completed = tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(completed.state, InvocationState::Succeeded);
    }
    assert_eq!(
        tokio::fs::read_to_string(&launches)
            .await
            .unwrap()
            .lines()
            .count(),
        1
    );

    let rejected = InvocationRequest::new(workspace, BazelCommand::Clean, Vec::new());
    let rejected_id = rejected.id;
    assert!(
        service
            .submit(
                rejected,
                ResultDisposition::Deferred {
                    retrieval: DeferredRetrieval::InlineResult,
                    expires_at_ms: bazel_mcp_types::unix_timestamp_ms() + 60_000,
                },
            )
            .await
            .is_err()
    );
    assert!(
        service
            .test_support()
            .get_invocation(rejected_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn wait_uses_durable_recovery_when_no_live_publisher_exists() {
    let root = tempfile::tempdir().unwrap();
    let store_root = root.path().join("store");
    let request = InvocationRequest::new(
        root.path().join("workspace"),
        BazelCommand::Build,
        vec!["//:recovered".into()],
    );
    let id = request.id;
    {
        let store = Store::open(&store_root).await.unwrap();
        store
            .create_invocation(&InvocationRecord::queued(request))
            .await
            .unwrap();
    }

    let service = InvocationService::new(
        Store::open(&store_root).await.unwrap(),
        RunnerConfig::default(),
    )
    .unwrap();
    let recovered = tokio::time::timeout(
        Duration::from_secs(1),
        service.wait(id, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(recovered.state, InvocationState::Interrupted);
    assert_eq!(service.active_invocation_count().await, 0);
}

#[tokio::test]
async fn dropping_an_invocation_future_kills_the_entire_process_group() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let child_pid = root.path().join("child.pid");
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n(sleep 30) &\necho $! > '{}'\nwait\n",
        child_pid.display()
    );
    let store_root = root.path().join("configured-store");
    let service = configured_service(&root, &workspace, &script, |_| {}).await;
    let request = InvocationRequest::new(workspace, BazelCommand::Build, vec!["//:target".into()]);
    let id = request.id;
    let running = tokio::spawn({
        let service = service.clone();
        async move { service.run(request).await }
    });
    wait_for_path(&child_pid).await;
    let pid: i32 = tokio::fs::read_to_string(&child_pid)
        .await
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    for _ in 0..100 {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());

    drop(service);
    let reopened = Store::open(store_root).await.unwrap();
    assert_eq!(
        reopened.get_invocation(id).await.unwrap().state,
        InvocationState::Interrupted
    );
}

#[tokio::test]
async fn bounds_hundreds_of_pending_requests_with_an_explicit_queue_limit() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = configured_service(
        &root,
        &workspace,
        "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nsleep 30\n",
        |config| {
            config.global_concurrency = 1;
            config.maximum_pending_invocations = 8;
        },
    )
    .await;
    let mut accepted = Vec::new();
    for index in 0..8 {
        let request = InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Build,
            vec![format!("//:queued-{index}")],
        );
        let id = request.id;
        accepted.push((
            id,
            tokio::spawn({
                let service = service.clone();
                async move { service.run(request).await }
            }),
        ));
    }
    for (id, _) in &accepted {
        wait_for_invocation(&service, *id).await;
    }
    for index in 8..300 {
        let error = service
            .run(InvocationRequest::new(
                workspace.clone(),
                BazelCommand::Build,
                vec![format!("//:rejected-{index}")],
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, bazel_mcp_runner::RunnerError::QueueFull(8)));
    }
    for (id, task) in accepted {
        wait_for_invocation(&service, id).await;
        service.cancel(id).await.unwrap();
        let record = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(record.state, InvocationState::Cancelled);
    }
}

#[tokio::test]
async fn rejects_unsupported_bazel_versions_before_creating_evidence() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = configured_service(
        &root,
        &workspace,
        "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then echo 'bazel 7.6.1'; exit 0; fi\nexit 0\n",
        |_| {},
    )
    .await;
    let error = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//:target".into()],
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        bazel_mcp_runner::RunnerError::UnsupportedBazelVersion { detected: 7, .. }
    ));
}

#[tokio::test]
async fn version_probes_are_singleflight_per_key_without_cross_key_serialization() {
    let root = tempfile::tempdir().unwrap();
    let first_workspace = root.path().join("first-workspace");
    let second_workspace = root.path().join("second-workspace");
    tokio::fs::create_dir(&first_workspace).await.unwrap();
    tokio::fs::create_dir(&second_workspace).await.unwrap();
    tokio::fs::write(
        second_workspace.join("MODULE.bazel"),
        "module(name='second')\n",
    )
    .await
    .unwrap();
    let service = configured_service(
        &root,
        &first_workspace,
        r#"#!/bin/sh
if [ "${1:-}" = --version ]; then
  mkdir "$PWD/version-probe" 2>/dev/null || touch "$PWD/duplicate-version-probe"
  touch "$PWD/version-started"
  i=0
  while [ ! -f "$PWD/release-version" ] && [ "$i" -lt 500 ]; do
    i=$((i+1))
    sleep 0.01
  done
  echo 'bazel 9.1.0'
  exit 0
fi
exit 0
"#,
        |_| {},
    )
    .await;

    let first = tokio::spawn({
        let service = service.clone();
        let workspace = first_workspace.clone();
        async move {
            service
                .run(InvocationRequest::new(
                    workspace,
                    BazelCommand::Build,
                    vec!["//:first".into()],
                ))
                .await
        }
    });
    wait_for_path(&first_workspace.join("version-started")).await;

    let identical = tokio::spawn({
        let service = service.clone();
        let workspace = first_workspace.clone();
        async move {
            service
                .run(InvocationRequest::new(
                    workspace,
                    BazelCommand::Build,
                    vec!["//:identical".into()],
                ))
                .await
        }
    });
    let unrelated = tokio::spawn({
        let service = service.clone();
        let workspace = second_workspace.clone();
        async move {
            service
                .run(InvocationRequest::new(
                    workspace,
                    BazelCommand::Build,
                    vec!["//:unrelated".into()],
                ))
                .await
        }
    });

    let unrelated_started = tokio::time::timeout(
        Duration::from_millis(500),
        wait_for_path(&second_workspace.join("version-started")),
    )
    .await
    .is_ok();
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::fs::write(first_workspace.join("release-version"), "")
        .await
        .unwrap();
    tokio::fs::write(second_workspace.join("release-version"), "")
        .await
        .unwrap();

    for result in [first.await, identical.await, unrelated.await] {
        assert_eq!(result.unwrap().unwrap().state, InvocationState::Succeeded);
    }
    assert!(
        unrelated_started,
        "an unrelated version probe was serialized behind the first key"
    );
    assert!(
        !first_workspace.join("duplicate-version-probe").exists(),
        "identical version probes were not coalesced"
    );
}

#[tokio::test]
async fn queued_cancellation_returns_a_complete_terminal_summary() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(&root, &workspace, "#!/bin/sh\nsleep 0.5\nexit 0\n").await;

    let mut blockers = Vec::new();
    for _ in 0..4 {
        let request =
            InvocationRequest::new(workspace.clone(), BazelCommand::Build, vec!["//...".into()]);
        let service = service.clone();
        blockers.push(tokio::spawn(
            async move { service.run(request).await.unwrap() },
        ));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let queued_request =
        InvocationRequest::new(workspace, BazelCommand::Build, vec!["//queued".into()]);
    let queued_id = queued_request.id;
    let queued = tokio::spawn({
        let service = service.clone();
        async move { service.run(queued_request).await.unwrap() }
    });
    wait_for_invocation(&service, queued_id).await;
    assert!(
        service
            .cancel(queued_id)
            .await
            .unwrap()
            .cancellation_requested
    );
    let record = tokio::time::timeout(Duration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.state, InvocationState::Cancelled);
    assert!(record.summary.is_some());

    for blocker in blockers {
        blocker.await.unwrap();
    }
}

#[tokio::test]
async fn scheduler_uses_effective_output_base_and_never_evaluates_arguments() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first-workspace");
    let second = root.path().join("second-workspace");
    let independent_first = root.path().join("independent-first-started");
    let independent_second = root.path().join("independent-second-started");
    let shared_first = root.path().join("shared-first-started");
    let shared_second = root.path().join("shared-second-started");
    let shared_release = root.path().join("shared-release");
    tokio::fs::create_dir(&first).await.unwrap();
    tokio::fs::create_dir(&second).await.unwrap();
    tokio::fs::write(second.join("MODULE.bazel"), "module(name='second')\n")
        .await
        .unwrap();
    let script = format!(
        "#!/bin/sh\n\
if [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n\
wait_for() {{\n\
  i=0\n\
  while [ \"$i\" -lt 500 ]; do\n\
    [ -f \"$1\" ] && return 0\n\
    i=$((i+1))\n\
    sleep 0.01\n\
  done\n\
  return 1\n\
}}\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    //:warm-version-cache) exit 0 ;;\n\
    //:independent-first)\n\
      touch '{}'\n\
      wait_for '{}'\n\
      exit $?\n\
      ;;\n\
    //:independent-second)\n\
      touch '{}'\n\
      wait_for '{}'\n\
      exit $?\n\
      ;;\n\
    //:shared-first)\n\
      touch '{}'\n\
      wait_for '{}'\n\
      exit $?\n\
      ;;\n\
    //:shared-second)\n\
      touch '{}'\n\
      wait_for '{}'\n\
      exit $?\n\
      ;;\n\
  esac\n\
done\n\
sleep 0.3\n\
exit 0\n",
        independent_first.display(),
        independent_second.display(),
        independent_second.display(),
        independent_first.display(),
        shared_first.display(),
        shared_release.display(),
        shared_second.display(),
        shared_release.display(),
    );
    let service = service(&root, &first, &script).await;

    service
        .run(InvocationRequest::new(
            first.clone(),
            BazelCommand::Build,
            vec!["//:warm-version-cache".into()],
        ))
        .await
        .unwrap();

    let shell_marker = root.path().join("shell-evaluated");
    let literal_argument = format!("$(touch {})", shell_marker.display());
    let (first_result, second_result) = tokio::join!(
        service.run(InvocationRequest::new(
            first.clone(),
            BazelCommand::Build,
            vec![literal_argument, "//:independent-first".into()],
        )),
        service.run(InvocationRequest::new(
            second.clone(),
            BazelCommand::Build,
            vec!["//:independent-second".into()],
        )),
    );
    assert_eq!(first_result.unwrap().state, InvocationState::Succeeded);
    assert_eq!(second_result.unwrap().state, InvocationState::Succeeded);
    assert!(independent_first.exists());
    assert!(independent_second.exists());
    assert!(!shell_marker.exists());

    let shared_output_base = root.path().join("shared-output-base");
    let startup_argument = format!("--output_base={}", shared_output_base.display());
    let mut first_request = InvocationRequest::new(
        first.clone(),
        BazelCommand::Build,
        vec!["//:shared-first".into()],
    );
    first_request.startup_arguments = vec![startup_argument.clone()];
    let mut second_request = InvocationRequest::new(
        second.clone(),
        BazelCommand::Build,
        vec!["//:shared-second".into()],
    );
    second_request.startup_arguments = vec![startup_argument];
    let first_id = first_request.id;
    let second_id = second_request.id;
    let first_run = tokio::spawn({
        let service = service.clone();
        async move { service.run(first_request).await.unwrap() }
    });
    let second_run = tokio::spawn({
        let service = service.clone();
        async move { service.run(second_request).await.unwrap() }
    });
    wait_for_invocation(&service, first_id).await;
    wait_for_invocation(&service, second_id).await;

    let one_started = tokio::time::timeout(Duration::from_secs(2), async {
        while !shared_first.exists() && !shared_second.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    let overlap_before_release = match (shared_first.exists(), shared_second.exists()) {
        (true, false) => {
            tokio::time::timeout(Duration::from_secs(1), wait_for_path(&shared_second))
                .await
                .is_ok()
        }
        (false, true) => tokio::time::timeout(Duration::from_secs(1), wait_for_path(&shared_first))
            .await
            .is_ok(),
        (true, true) => true,
        (false, false) => false,
    };
    tokio::fs::write(&shared_release, b"release").await.unwrap();
    let (first_result, second_result) = tokio::join!(first_run, second_run);

    assert!(one_started, "neither shared output-base request started");
    assert!(
        !overlap_before_release,
        "shared output-base requests overlapped before the active request was released"
    );
    assert_eq!(first_result.unwrap().state, InvocationState::Succeeded);
    assert_eq!(second_result.unwrap().state, InvocationState::Succeeded);
    assert!(shared_first.exists());
    assert!(shared_second.exists());

    let mut same_workspace = Vec::new();
    for _ in 0..4 {
        let request =
            InvocationRequest::new(first.clone(), BazelCommand::Build, vec!["//...".into()]);
        let service = service.clone();
        same_workspace.push(tokio::spawn(
            async move { service.run(request).await.unwrap() },
        ));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let independent = service.run(InvocationRequest::new(
        second,
        BazelCommand::Build,
        vec!["//...".into()],
    ));
    tokio::time::timeout(Duration::from_secs(2), independent)
        .await
        .expect("same-workspace waiters exhausted the global concurrency pool")
        .unwrap();
    for run in same_workspace {
        run.await.unwrap();
    }
}

#[tokio::test]
async fn independent_services_serialize_each_request_by_explicit_output_base() {
    let root = tempfile::tempdir().unwrap();
    let first_workspace = root.path().join("first-workspace");
    let second_workspace = root.path().join("second-workspace");
    let first_started = root.path().join("first-started");
    let first_release = root.path().join("first-release");
    let second_started = root.path().join("second-started");
    tokio::fs::create_dir(&first_workspace).await.unwrap();
    tokio::fs::create_dir(&second_workspace).await.unwrap();
    let executable = root.path().join("shared-fake-bazel");
    let script = format!(
        "#!/bin/sh\n\
if [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    //:first)\n\
      touch '{}'\n\
      while [ ! -f '{}' ]; do sleep 0.01; done\n\
      exit 0\n\
      ;;\n\
    //:second) touch '{}'; exit 0 ;;\n\
  esac\n\
done\n\
exit 0\n",
        first_started.display(),
        first_release.display(),
        second_started.display(),
    );
    tokio::fs::write(&executable, script).await.unwrap();
    tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let lock_root = root.path().join("shared-output-base-locks");
    let first_service = shared_executable_service(
        &root,
        &first_workspace,
        &executable,
        "first-store",
        &lock_root,
    )
    .await;
    let second_service = shared_executable_service(
        &root,
        &second_workspace,
        &executable,
        "second-store",
        &lock_root,
    )
    .await;
    let output_base = root.path().join("shared-output-base");
    let startup_argument = format!("--output_base={}", output_base.display());
    let mut first_request = InvocationRequest::new(
        first_workspace,
        BazelCommand::Build,
        vec!["//:first".to_owned()],
    );
    first_request.startup_arguments = vec![startup_argument.clone()];
    let first_run = tokio::spawn({
        let service = first_service.clone();
        async move { service.run(first_request).await.unwrap() }
    });
    wait_for_path(&first_started).await;

    let mut second_request = InvocationRequest::new(
        second_workspace,
        BazelCommand::Build,
        vec!["//:second".to_owned()],
    );
    second_request.startup_arguments = vec![startup_argument];
    let second_id = second_request.id;
    let second_run = tokio::spawn({
        let service = second_service.clone();
        async move { service.run(second_request).await.unwrap() }
    });
    wait_for_invocation(&second_service, second_id).await;
    let progress = wait_for_output_base_wait(&second_service, second_id).await;

    assert!(
        progress
            .output_base_lock_owner
            .as_deref()
            .is_some_and(|owner| owner.starts_with("bazel_mcp:pid="))
    );
    assert!(
        !second_started.exists(),
        "the second Bazel client started before the first released the shared output base"
    );
    tokio::fs::write(&first_release, b"release").await.unwrap();
    let first_record = first_run.await.unwrap();
    let second_record = second_run.await.unwrap();

    assert_eq!(first_record.state, InvocationState::Succeeded);
    assert_eq!(second_record.state, InvocationState::Succeeded);
    assert!(second_started.exists());
    assert!(second_record.metrics.output_base_lock_wait_ms > 0);
}

#[tokio::test]
async fn cancellation_while_waiting_for_cross_process_output_base_lock_never_spawns_bazel() {
    let root = tempfile::tempdir().unwrap();
    let first_workspace = root.path().join("first-workspace");
    let second_workspace = root.path().join("second-workspace");
    let first_started = root.path().join("first-started");
    let first_release = root.path().join("first-release");
    let second_started = root.path().join("second-started");
    tokio::fs::create_dir(&first_workspace).await.unwrap();
    tokio::fs::create_dir(&second_workspace).await.unwrap();
    let executable = root.path().join("cancellable-fake-bazel");
    let script = format!(
        "#!/bin/sh\n\
if [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n\
for arg in \"$@\"; do\n\
  case \"$arg\" in\n\
    //:first)\n\
      touch '{}'\n\
      while [ ! -f '{}' ]; do sleep 0.01; done\n\
      exit 0\n\
      ;;\n\
    //:second) touch '{}'; exit 0 ;;\n\
  esac\n\
done\n\
exit 0\n",
        first_started.display(),
        first_release.display(),
        second_started.display(),
    );
    tokio::fs::write(&executable, script).await.unwrap();
    tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .await
        .unwrap();
    let lock_root = root.path().join("shared-output-base-locks");
    let first_service = shared_executable_service(
        &root,
        &first_workspace,
        &executable,
        "first-store",
        &lock_root,
    )
    .await;
    let second_service = shared_executable_service(
        &root,
        &second_workspace,
        &executable,
        "second-store",
        &lock_root,
    )
    .await;
    let output_base = root.path().join("shared-output-base");
    let startup_argument = format!("--output_base={}", output_base.display());
    let mut first_request = InvocationRequest::new(
        first_workspace,
        BazelCommand::Build,
        vec!["//:first".to_owned()],
    );
    first_request.startup_arguments = vec![startup_argument.clone()];
    let first_run = tokio::spawn({
        let service = first_service.clone();
        async move { service.run(first_request).await.unwrap() }
    });
    wait_for_path(&first_started).await;

    let mut second_request = InvocationRequest::new(
        second_workspace,
        BazelCommand::Build,
        vec!["//:second".to_owned()],
    );
    second_request.startup_arguments = vec![startup_argument];
    let second_id = second_request.id;
    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.clone();
    let second_run = tokio::spawn({
        let service = second_service.clone();
        async move {
            service
                .run_with_cancellation(second_request, run_cancellation)
                .await
                .unwrap()
        }
    });
    wait_for_invocation(&second_service, second_id).await;
    wait_for_output_base_wait(&second_service, second_id).await;
    cancellation.cancel();
    let second_record = second_run.await.unwrap();

    assert_eq!(second_record.state, InvocationState::Cancelled);
    assert!(!second_started.exists());
    tokio::fs::write(&first_release, b"release").await.unwrap();
    assert_eq!(first_run.await.unwrap().state, InvocationState::Succeeded);
}

#[tokio::test]
async fn native_bazel_output_base_wait_is_reported_and_included_in_metrics() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let wait_started = root.path().join("native-wait-started");
    let release = root.path().join("native-wait-release");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let script = format!(
        "#!/bin/sh\n\
if [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n\
echo 'WARNING: Running Bazel server needs to be killed, because startup options are different:' >&2\n\
echo 'Another command holds the output base lock:' >&2\n\
echo 'owner=client' >&2\n\
echo 'Waiting for it to complete...' >&2\n\
touch '{}'\n\
while [ ! -f '{}' ]; do sleep 0.01; done\n\
echo 'Starting local Bazel server and connecting to it...' >&2\n\
exit 0\n",
        wait_started.display(),
        release.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;
    let request =
        InvocationRequest::new(workspace, BazelCommand::Build, vec!["//:target".to_owned()]);
    let id = request.id;
    let run = tokio::spawn({
        let service = service.clone();
        async move { service.run(request).await.unwrap() }
    });
    wait_for_path(&wait_started).await;
    let progress = wait_for_output_base_wait(&service, id).await;

    assert_eq!(
        progress.output_base_lock_owner.as_deref(),
        Some("bazel_client")
    );
    tokio::fs::write(&release, b"release").await.unwrap();
    let record = run.await.unwrap();

    assert_eq!(record.state, InvocationState::Succeeded);
    assert!(record.metrics.output_base_lock_wait_ms > 0);
}

#[tokio::test]
async fn explicit_output_base_owner_is_reported_without_bazel_stderr_markers() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let output_base = root.path().join("shared-output-base");
    let bazel_started = root.path().join("bazel-started");
    let release = root.path().join("release");
    tokio::fs::create_dir(&workspace).await.unwrap();
    tokio::fs::create_dir(&output_base).await.unwrap();
    let script = format!(
        "#!/bin/sh\n\
if [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\n\
touch '{}'\n\
while [ ! -f '{}' ]; do sleep 0.01; done\n\
exit 0\n",
        bazel_started.display(),
        release.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;
    let mut holder_command = tokio::process::Command::new("sleep");
    holder_command.arg("30").kill_on_drop(true);
    let mut holder = holder_command.spawn().unwrap();
    let holder_pid = holder.id().unwrap();
    tokio::fs::write(
        output_base.join("lock"),
        format!("pid={holder_pid}\nowner=client\ncwd=/private/workspace\n"),
    )
    .await
    .unwrap();
    let mut request =
        InvocationRequest::new(workspace, BazelCommand::Build, vec!["//:target".to_owned()]);
    request.startup_arguments = vec![format!("--output_base={}", output_base.display())];
    let id = request.id;
    let run = tokio::spawn({
        let service = service.clone();
        async move { service.run(request).await.unwrap() }
    });
    wait_for_path(&bazel_started).await;
    let progress = wait_for_output_base_wait(&service, id).await;

    let expected_owner = format!("bazel_client:pid={holder_pid}");
    assert_eq!(
        progress.output_base_lock_owner.as_deref(),
        Some(expected_owner.as_str())
    );
    holder.kill().await.unwrap();
    let _ = holder.wait().await;
    tokio::fs::write(&release, b"release").await.unwrap();
    let record = run.await.unwrap();

    assert_eq!(record.state, InvocationState::Succeeded);
    assert!(record.metrics.output_base_lock_wait_ms > 0);
}

#[tokio::test]
async fn server_owned_flags_precede_the_target_argument_delimiter() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\nseen_bep=0\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) seen_bep=1 ;;\n    --) [ \"$seen_bep\" -eq 1 ] || exit 42 ;;\n  esac\ndone\nexit 0\n",
    )
    .await;
    let request = InvocationRequest::new(
        workspace,
        BazelCommand::Build,
        vec!["//:target".into(), "--".into(), "--test_arg=value".into()],
    );

    let record = service.run(request).await.unwrap();
    assert_eq!(record.state, InvocationState::Succeeded);
}

#[tokio::test]
async fn preserves_user_bes_flags_while_adding_the_private_bep_file() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\nseen_bes=0\nseen_bep=0\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --bes_backend=grpc://example.invalid:1985) seen_bes=1 ;;\n    --build_event_binary_file=*) seen_bep=1 ;;\n  esac\ndone\n[ \"$seen_bes\" -eq 1 ] && [ \"$seen_bep\" -eq 1 ]\n",
    )
    .await;
    let record = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec![
                "//:target".into(),
                "--bes_backend=grpc://example.invalid:1985".into(),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(record.state, InvocationState::Succeeded);
}

#[tokio::test]
async fn fifo_transport_spools_private_evidence_and_cleans_up_the_pipe() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let fixture = root.path().join("test-outcomes.bep");
    std::fs::write(&fixture, TEST_OUTCOMES_BEP).unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do [ \"$arg\" = info ] && is_info=1; done\nif [ \"${{is_info:-0}}\" = 1 ]; then echo '{}'; exit 0; fi\nfor arg in \"$@\"; do case \"$arg\" in --build_event_binary_file=*) bep_path=${{arg#*=}} ;; esac; done\ncp '{}' \"$bep_path\"\n",
        std::process::id(),
        fixture.display(),
    );
    let service = configured_service(&root, &workspace, &script, |config| {
        config.bep_transport = BepTransport::Fifo;
    })
    .await;

    let record = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//:target".into()],
        ))
        .await
        .unwrap();
    assert_eq!(record.state, InvocationState::Succeeded);
    let paths = service.test_support().paths_for(&record);
    assert_eq!(
        std::fs::read(&paths.bep).unwrap(),
        std::fs::read(fixture).unwrap()
    );
    assert!(!paths.bep.with_extension("bep.fifo").exists());
}

#[tokio::test]
async fn fifo_transport_falls_back_to_file_tail_when_pid_probe_fails() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let fixture = root.path().join("test-outcomes.bep");
    std::fs::write(&fixture, TEST_OUTCOMES_BEP).unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do [ \"$arg\" = info ] && exit 1; done\nfor arg in \"$@\"; do case \"$arg\" in --build_event_binary_file=*) bep_path=${{arg#*=}} ;; esac; done\n[ -f \"$bep_path\" ] || exit 42\ncp '{}' \"$bep_path\"\n",
        fixture.display(),
    );
    let service = configured_service(&root, &workspace, &script, |config| {
        config.bep_transport = BepTransport::Fifo;
    })
    .await;

    let record = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec!["//:target".into()],
        ))
        .await
        .unwrap();
    assert_eq!(record.state, InvocationState::Succeeded);
    assert_eq!(
        std::fs::read(service.test_support().paths_for(&record).bep).unwrap(),
        std::fs::read(fixture).unwrap()
    );
}

#[tokio::test]
async fn bes_transport_injects_the_loopback_backend_and_owns_upload_mode() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = configured_service(
        &root,
        &workspace,
        "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then echo 'bazel 9.1.0'; exit 0; fi\nseen_bes=0\nseen_wait=0\nseen_bep=0\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --bes_backend=grpc://127.0.0.1:*) seen_bes=1 ;;\n    --bes_upload_mode=wait_for_upload_complete) seen_wait=1 ;;\n    --build_event_binary_file=*) seen_bep=1 ;;\n  esac\ndone\n[ \"$seen_bes\" -eq 1 ] && [ \"$seen_wait\" -eq 1 ] && [ \"$seen_bep\" -eq 0 ]\n",
        |config| config.bep_transport = BepTransport::Bes,
    )
    .await;

    let record = service
        .run(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Build,
            vec!["//:target".into()],
        ))
        .await
        .unwrap();
    assert_eq!(record.state, InvocationState::Succeeded);

    let error = service
        .run(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Build,
            vec![
                "//:target".into(),
                "--bes_backend=grpc://remote.example:1985".into(),
            ],
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        bazel_mcp_runner::RunnerError::BesBackendConflict
    ));

    let error = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Build,
            vec![
                "//:target".into(),
                "--bes_upload_mode=nowait_for_upload_complete".into(),
            ],
        ))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        bazel_mcp_runner::RunnerError::BesUploadModeConflict
    ));
}

#[tokio::test]
async fn query_and_informational_commands_return_bounded_initial_evidence() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\nprintf '//pkg:first\\n//pkg:second\\n//pkg:third\\n//pkg:fourth\\n'\n",
    )
    .await;

    let query = service
        .run(InvocationRequest::new(
            workspace.clone(),
            BazelCommand::Query,
            vec!["//...".into()],
        ))
        .await
        .unwrap();
    let query_summary = query.summary.unwrap();
    assert_eq!(query_summary.query_result_count, Some(4));
    assert_eq!(query_summary.query_sample.len(), 3);
    assert_eq!(query_summary.inspect_hint, Some(InspectHint::QueryResults));

    let informational = service
        .run(InvocationRequest::new(
            workspace,
            BazelCommand::Version,
            Vec::new(),
        ))
        .await
        .unwrap();
    assert!(
        informational
            .summary
            .unwrap()
            .headline
            .contains("//pkg:first")
    );
    assert!(informational.metrics.raw_output_bytes > 0);
}

#[tokio::test]
async fn inspect_preserves_typed_nested_results_for_server_encoding() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(&root, &workspace, "#!/bin/sh\nexit 0\n").await;
    let request = InvocationRequest::new(workspace, BazelCommand::Test, vec!["//...".into()]);
    let id = request.id;
    service
        .test_support()
        .create_invocation(&InvocationRecord::queued(request))
        .await
        .unwrap();
    service
        .test_support()
        .transition(id, InvocationState::Starting, None, None)
        .await
        .unwrap();
    service
        .test_support()
        .transition(id, InvocationState::Running, None, None)
        .await
        .unwrap();
    let diagnostics = (0..20)
        .map(|index| Diagnostic {
            severity: Severity::Error,
            category: DiagnosticCategory::Test,
            message: format!("diagnostic-{index}-{}", "x".repeat(980)),
            location: None,
            target: Some(format!("//very/long/package:{index}-{}", "t".repeat(900))),
            action: Some("compile".repeat(100)),
            repetition_count: 1,
        })
        .collect();
    let cases = (0..20)
        .map(|index| TestCase {
            name: format!("case-{index}"),
            status: TestStatus::Failed,
            duration_ms: Some(1),
            message: Some("failure".repeat(140)),
        })
        .collect();
    let summary = InvocationSummary {
        diagnostics,
        tests: vec![TestResult {
            label: "//pkg:test".into(),
            status: TestStatus::Failed,
            duration_ms: Some(20),
            attempts: 1,
            shard: None,
            cases,
            test_log_available: false,
            test_log_unavailable_reason: Some("test_log_not_found".into()),
        }],
        ..InvocationSummary::default()
    };
    service
        .test_support()
        .transition(
            id,
            InvocationState::Failed,
            Some(Termination::Exit { code: 1 }),
            Some(summary),
        )
        .await
        .unwrap();

    for view in [InspectView::Summary, InspectView::Tests] {
        let result = service
            .inspect(InspectRequest {
                invocation_id: Some(id),
                workspace: None,
                state: None,
                command: None,
                view,
                cursor: None,
                filter: None,
                item_limit: 20,
                scan_limit: 10_000,
            })
            .await
            .unwrap();
        assert!(!result.truncated);
        assert!(serde_json::to_vec(&result).unwrap().len() > 8 * 1024);
    }
    let unavailable = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::TestLog,
            cursor: None,
            filter: Some("//PKG:TEST".into()),
            item_limit: 20,
            scan_limit: 10_000,
        })
        .await
        .unwrap();
    assert_eq!(
        unavailable.items,
        InspectPayload::TestLog(vec!["//pkg:test: test_log_not_found".to_owned()])
    );
}

#[tokio::test]
async fn postprocessing_failures_do_not_leave_invocations_running() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let invocation_root = root.path().join("store/invocations");
    let script = format!(
        "#!/bin/sh\necho '//pkg:target'\nrm -rf '{}'\nexit 0\n",
        invocation_root.display()
    );
    let service = service(&root, &workspace, &script).await;
    let request = InvocationRequest::new(workspace, BazelCommand::Query, vec!["//...".into()]);
    let id = request.id;

    let record = service.run(request).await.unwrap();
    assert_eq!(record.request.id, id);
    assert!(record.state.is_terminal());
    assert_eq!(record.state, InvocationState::Failed);
    assert!(record.summary.is_some());
}

#[tokio::test]
async fn evidence_file_failures_become_terminal_instead_of_stranding_starting_rows() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(
        &root,
        &workspace,
        "#!/bin/sh\ntrap 'exit 130' INT TERM\nsleep 30\n",
    )
    .await;
    let blocker_request = InvocationRequest::new(
        workspace.clone(),
        BazelCommand::Build,
        vec!["//:blocker".into()],
    );
    let blocker_id = blocker_request.id;
    let blocker = tokio::spawn({
        let service = service.clone();
        async move { service.run(blocker_request).await }
    });
    loop {
        if service
            .test_support()
            .get_invocation(blocker_id)
            .await
            .is_ok_and(|record| record.state == InvocationState::Running)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let request =
        InvocationRequest::new(workspace, BazelCommand::Build, vec!["//:disk-full".into()]);
    let id = request.id;
    let queued = tokio::spawn({
        let service = service.clone();
        async move { service.run(request).await }
    });
    let paths = loop {
        if let Ok(record) = service.test_support().get_invocation(id).await {
            break service.test_support().paths_for(&record);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    tokio::fs::create_dir(&paths.stdout).await.unwrap();
    service.cancel(blocker_id).await.unwrap();
    assert_eq!(
        blocker.await.unwrap().unwrap().state,
        InvocationState::Cancelled
    );

    let record = queued.await.unwrap().unwrap();
    assert_eq!(record.state, InvocationState::Failed);
    assert!(matches!(
        record.termination,
        Some(Termination::SpawnFailure { .. })
    ));
    assert!(
        record
            .summary
            .unwrap()
            .headline
            .contains("Could not prepare Bazel invocation")
    );
}
