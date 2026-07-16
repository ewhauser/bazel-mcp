#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};

use bazel_mcp_bep::proto::{
    BuildEvent, BuildEventId, File, TestResult as BepTestResult, TestSummary as BepTestSummary,
    build_event, build_event_id, file,
};
use bazel_mcp_bep::{encode_event_id, encode_frame};
use bazel_mcp_policy::PolicyConfig;
use bazel_mcp_runner::{InspectRequest, InspectView, InvocationService, RunnerConfig};
use bazel_mcp_store::Store;
use bazel_mcp_types::{
    Artifact, ArtifactKind, BazelCommand, DeferredRetrieval, Diagnostic, DiagnosticCategory,
    InvocationRecord, InvocationRequest, InvocationState, InvocationSummary, ResultDisposition,
    Severity, Termination, TestCase, TestResult, TestStatus,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

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
        if service.store().get_invocation(id).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for invocation {id}");
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
    InvocationService::new(
        Store::open(root.path().join("configured-store"))
            .await
            .unwrap(),
        config,
    )
    .unwrap()
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
                limit: 20,
                max_bytes: 8 * 1024,
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
            limit: 20,
            max_bytes: 8 * 1024,
        })
        .await
        .unwrap();
    assert!(secret_filter.items.as_array().unwrap().is_empty());

    service
        .store()
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
            limit: 20,
            max_bytes: 8 * 1024,
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
            limit: 20,
            max_bytes: 1024,
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
            limit: 20,
            max_bytes: 1024,
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
                limit: 20,
                max_bytes: 1024,
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
            limit: 20,
            max_bytes: 1024,
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
            limit: 20,
            max_bytes: 1024,
        })
        .await
        .unwrap();
    assert_eq!(filtered.items.as_array().unwrap().len(), 1);
    assert!(filtered.items[0].as_str().unwrap().contains("line-150"));

    let unmatched = service
        .inspect(InspectRequest {
            invocation_id: Some(id),
            workspace: None,
            state: None,
            command: None,
            view: InspectView::Log,
            cursor: None,
            filter: Some("not-present".into()),
            limit: 20,
            max_bytes: 1024,
        })
        .await
        .unwrap();
    assert!(unmatched.items.as_array().unwrap().is_empty());
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
        "setup\nassertion error: SNAPSHOT_TEST_ROOT_CAUSE\n",
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
        "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo 'bazel 9.1.0'; exit 0; fi\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --build_event_binary_file=*) bep_path=${{arg#*=}} ;;\n    --test_output=errors) touch '{}' ;;\n  esac\ndone\nif [ -f '{}' ]; then : > \"$bep_path\"; exit 0; fi\ncp '{}' \"$bep_path\"\ntouch '{}'\necho 'SNAPSHOT_TEST_ROOT_CAUSE'\nexit 1\n",
        flag_marker.display(),
        failed_once.display(),
        bep.display(),
        failed_once.display(),
    );
    let service = configured_service(&root, &workspace, &script, |_| {}).await;

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
    assert!(summary.headline.contains("SNAPSHOT_TEST_ROOT_CAUSE"));
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
            limit: 20,
            max_bytes: 8 * 1024,
        })
        .await
        .unwrap();
    let items = inspected.items.as_array().unwrap();
    assert!(items.iter().all(serde_json::Value::is_string));
    assert!(
        items
            .iter()
            .any(|item| item.as_str().unwrap().contains("SNAPSHOT_TEST_ROOT_CAUSE"))
    );

    let failed_paths = service.store().paths_for(&failed);
    let retained_before = tokio::fs::read(&failed_paths.test_logs_raw).await.unwrap();
    assert!(String::from_utf8_lossy(&retained_before).contains("SNAPSHOT_TEST_ROOT_CAUSE"));
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

    let completed = tokio::time::timeout(
        Duration::from_secs(3),
        service.wait(id, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(completed.state, InvocationState::Succeeded);
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
    assert!(service.store().get_invocation(rejected_id).await.is_err());
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
        "#!/bin/sh\nif [ \"${1:-}\" = --version ]; then echo 'bazel 6.5.0'; exit 0; fi\nexit 0\n",
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
        bazel_mcp_runner::RunnerError::UnsupportedBazelVersion { detected: 6, .. }
    ));
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
    tokio::fs::create_dir(&first).await.unwrap();
    tokio::fs::create_dir(&second).await.unwrap();
    tokio::fs::write(second.join("MODULE.bazel"), "module(name='second')\n")
        .await
        .unwrap();
    let service = service(&root, &first, "#!/bin/sh\nsleep 0.3\nexit 0\n").await;

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
    let independent_started = Instant::now();
    let (first_result, second_result) = tokio::join!(
        service.run(InvocationRequest::new(
            first.clone(),
            BazelCommand::Build,
            vec![literal_argument],
        )),
        service.run(InvocationRequest::new(
            second.clone(),
            BazelCommand::Build,
            vec!["//...".into()],
        )),
    );
    let independent_elapsed = independent_started.elapsed();
    assert_eq!(first_result.unwrap().state, InvocationState::Succeeded);
    assert_eq!(second_result.unwrap().state, InvocationState::Succeeded);
    assert!(!shell_marker.exists());

    let shared_output_base = root.path().join("shared-output-base");
    let startup_argument = format!("--output_base={}", shared_output_base.display());
    let mut first_request =
        InvocationRequest::new(first.clone(), BazelCommand::Build, vec!["//...".into()]);
    first_request.startup_arguments = vec![startup_argument.clone()];
    let mut second_request =
        InvocationRequest::new(second.clone(), BazelCommand::Build, vec!["//...".into()]);
    second_request.startup_arguments = vec![startup_argument];
    let serialized_started = Instant::now();
    let (first_result, second_result) =
        tokio::join!(service.run(first_request), service.run(second_request),);
    let serialized_elapsed = serialized_started.elapsed();
    assert_eq!(first_result.unwrap().state, InvocationState::Succeeded);
    assert_eq!(second_result.unwrap().state, InvocationState::Succeeded);
    assert!(
        serialized_elapsed > independent_elapsed + Duration::from_millis(200),
        "shared output base did not serialize: independent={independent_elapsed:?}, shared={serialized_elapsed:?}"
    );

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
    assert!(query_summary.inspect_hint.as_deref() == Some("query_results"));

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
async fn inspect_shrinks_nested_results_to_the_hard_byte_budget() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir(&workspace).await.unwrap();
    let service = service(&root, &workspace, "#!/bin/sh\nexit 0\n").await;
    let request = InvocationRequest::new(workspace, BazelCommand::Test, vec!["//...".into()]);
    let id = request.id;
    service
        .store()
        .create_invocation(&InvocationRecord::queued(request))
        .await
        .unwrap();
    service
        .store()
        .transition(id, InvocationState::Starting, None, None)
        .await
        .unwrap();
    service
        .store()
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
        .store()
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
                limit: 20,
                max_bytes: 8 * 1024,
            })
            .await
            .unwrap();
        assert!(result.truncated);
        assert!(serde_json::to_vec(&result).unwrap().len() <= 8 * 1024);
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
            limit: 20,
            max_bytes: 8 * 1024,
        })
        .await
        .unwrap();
    assert_eq!(
        unavailable.items,
        serde_json::json!(["//pkg:test: test_log_not_found"])
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
            .store()
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
        if let Ok(record) = service.store().get_invocation(id).await {
            break service.store().paths_for(&record);
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
