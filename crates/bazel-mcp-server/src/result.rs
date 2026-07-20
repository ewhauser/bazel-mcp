//! Final MCP result encoding and byte-budget enforcement.

use bazel_mcp_types::{
    AvailableViews, Diagnostic, InspectHint, InspectPayload, InspectResult, InvocationRecord,
    InvocationState, QueryRow, RunSummary, TargetCounts, Termination, TestCounts,
};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;

use crate::ResultEncoding;

#[derive(Clone, Debug)]
pub(crate) struct ResultEncoder {
    encoding: ResultEncoding,
}

#[derive(Clone, Debug)]
pub(crate) struct RunResultBuilder {
    encoder: ResultEncoder,
}

#[derive(Debug)]
pub(crate) struct EncodedResult {
    pub(crate) result: CallToolResult,
    pub(crate) visible_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExecutionResult<'a> {
    record: &'a InvocationRecord,
    tool_error: bool,
    retained: bool,
}

impl<'a> ExecutionResult<'a> {
    #[must_use]
    pub(crate) const fn new(record: &'a InvocationRecord, tool_error: bool) -> Self {
        Self {
            record,
            tool_error,
            retained: true,
        }
    }

    #[must_use]
    const fn ephemeral(record: &'a InvocationRecord) -> Self {
        Self {
            record,
            tool_error: false,
            retained: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct RunResult {
    invocation_id: String,
    state: InvocationState,
    command: String,
    exit_code: Option<i32>,
    duration_ms: u64,
    headline: String,
    targets: TargetCounts,
    tests: TestCounts,
    diagnostics: Vec<Diagnostic>,
    query_result_count: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    query_sample: Vec<QueryRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<RunSummary>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    inspect_hint: Option<InspectHint>,
    available_views: AvailableViews,
    more_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rerun_hint: Option<&'static str>,
}

impl ResultEncoder {
    #[must_use]
    pub(crate) const fn new(encoding: ResultEncoding) -> Self {
        Self { encoding }
    }

    pub(crate) fn encode<T: Serialize>(&self, value: &T) -> Result<EncodedResult, String> {
        let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
        self.encode_value(value)
    }

    pub(crate) fn encode_value(&self, value: serde_json::Value) -> Result<EncodedResult, String> {
        let (result, visible_bytes) = match self.encoding {
            ResultEncoding::Text => {
                let text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
                let bytes = text.len();
                (
                    CallToolResult::success(vec![ContentBlock::text(text)]),
                    bytes,
                )
            }
            ResultEncoding::Toon => {
                let text = toon_format::encode_default(&value)
                    .map_err(|error| format!("encode TOON result: {error}"))?;
                let bytes = text.len();
                (
                    CallToolResult::success(vec![ContentBlock::text(text)]),
                    bytes,
                )
            }
            ResultEncoding::Structured => {
                let bytes = serde_json::to_vec(&value)
                    .map_err(|error| error.to_string())?
                    .len();
                let mut result = CallToolResult::default();
                result.structured_content = Some(value);
                result.is_error = Some(false);
                (result, bytes)
            }
            ResultEncoding::Both => {
                let text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
                let structured_bytes = serde_json::to_vec(&value)
                    .map_err(|error| error.to_string())?
                    .len();
                let visible_bytes = text.len().saturating_add(structured_bytes);
                let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
                result.structured_content = Some(value);
                (result, visible_bytes)
            }
        };
        Ok(EncodedResult {
            result,
            visible_bytes,
        })
    }

    pub(crate) fn encode_inspect(
        &self,
        mut value: InspectResult,
        limit: usize,
    ) -> Result<EncodedResult, String> {
        loop {
            let encoded = self.encode(&value)?;
            if encoded.visible_bytes <= limit {
                return Ok(encoded);
            }
            if !shrink_inspect(&mut value) {
                return Err(
                    "bounded bazel.inspect response could not fit its hard byte limit".into(),
                );
            }
        }
    }
}

impl RunResultBuilder {
    #[must_use]
    pub(crate) const fn new(encoder: ResultEncoder) -> Self {
        Self { encoder }
    }

    pub(crate) fn build(&self, execution: ExecutionResult<'_>) -> Result<EncodedResult, String> {
        let ExecutionResult {
            record,
            tool_error,
            retained,
        } = execution;
        let exit_code = match &record.termination {
            Some(Termination::Exit { code }) => Some(*code),
            _ => None,
        };
        let summary = record
            .summary
            .as_ref()
            .ok_or_else(|| "completed invocation has no summary".to_owned())?;
        let follow_up_available = summary.truncated
            || !summary.targets.is_empty()
            || !summary.tests.is_empty()
            || summary.inspect_hint.is_some();
        let mut result = RunResult {
            invocation_id: record.request.id.to_string(),
            state: record.state,
            command: record.request.command.as_str().to_owned(),
            exit_code,
            duration_ms: record.metrics.bazel_wall_ms,
            headline: summary.headline.clone(),
            targets: summary.target_counts.clone(),
            tests: summary.test_counts.clone(),
            diagnostics: summary.diagnostics.clone(),
            query_result_count: summary.query_result_count,
            query_sample: summary.query_sample.clone(),
            run: record.run.clone(),
            truncated: summary.truncated,
            inspect_hint: if retained { summary.inspect_hint } else { None },
            available_views: if retained {
                AvailableViews::follow_up()
            } else {
                AvailableViews::none()
            },
            more_available: retained && follow_up_available,
            rerun_hint: (!retained && follow_up_available)
                .then_some("rerun with --no-agent-mode for unfiltered Bazel output"),
        };
        let limit = if summary.success { 2 * 1024 } else { 8 * 1024 };
        loop {
            let mut encoded = self.encoder.encode(&result)?;
            if encoded.visible_bytes <= limit {
                if tool_error {
                    encoded.result.is_error = Some(true);
                }
                return Ok(encoded);
            }
            result.truncated = true;
            result.more_available = retained;
            if !retained {
                result.rerun_hint = Some("rerun with --no-agent-mode for unfiltered Bazel output");
            }
            if result
                .run
                .as_mut()
                .is_some_and(|run| run.output_excerpt.pop().is_some())
                || result.query_sample.pop().is_some()
                || result.diagnostics.pop().is_some()
            {
                continue;
            }
            if shrink_utf8(&mut result.headline, 96) || shrink_utf8(&mut result.command, 32) {
                continue;
            }
            return Err("bounded bazel.run response could not fit its hard byte limit".into());
        }
    }

    pub(crate) fn build_cli(&self, record: &InvocationRecord) -> Result<String, String> {
        let encoded = self.build(ExecutionResult::ephemeral(record))?;
        if let Some(ContentBlock::Text(text)) = encoded.result.content.first() {
            return Ok(text.text.clone());
        }
        encoded
            .result
            .structured_content
            .as_ref()
            .ok_or_else(|| "encoded result has no CLI representation".to_owned())
            .and_then(|value| serde_json::to_string(value).map_err(|error| error.to_string()))
    }
}

fn shrink_inspect(result: &mut InspectResult) -> bool {
    match &mut result.items {
        InspectPayload::Summary(items) => {
            if let Some(summary) = items.last_mut() {
                summary.truncated = true;
                result.truncated = true;
                if summary.query_sample.pop().is_some() || summary.diagnostics.pop().is_some() {
                    return true;
                }
                if shrink_utf8(&mut summary.headline, 64) {
                    return true;
                }
            }
        }
        InspectPayload::Tests(items) => {
            if let Some(test) = items.iter_mut().rev().find(|test| !test.cases.is_empty()) {
                test.cases.pop();
                result.truncated = true;
                return true;
            }
        }
        _ => {}
    }

    let len = result.items.len();
    if len == 0 {
        return false;
    }
    result.truncate_items(len - 1);
    true
}

fn shrink_utf8(value: &mut String, minimum: usize) -> bool {
    if value.len() <= minimum {
        return false;
    }
    let target = (value.len() * 3 / 4).max(minimum);
    let mut boundary = target.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push('…');
    true
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bazel_mcp_types::{
        BazelCommand, InspectPayload, InspectResult, InspectView, InvocationRecord,
        InvocationRequest, InvocationState, InvocationSummary, RunOutcome, RunSummary, Termination,
    };
    use rmcp::model::ContentBlock;

    use super::{ExecutionResult, ResultEncoder, RunResultBuilder, shrink_utf8};
    use crate::ResultEncoding;

    #[test]
    fn utf8_shrinking_stops_on_a_character_boundary() {
        let mut value = "é".repeat(100);
        assert!(shrink_utf8(&mut value, 16));
        assert!(value.ends_with('…'));
        assert!(value.len() < 203);
    }

    #[test]
    fn oversized_ephemeral_results_offer_a_rerun_without_promising_inspection() {
        let request = InvocationRequest::new(
            PathBuf::from("/workspace"),
            BazelCommand::Build,
            vec!["//:target".to_owned()],
        );
        let mut record = InvocationRecord::queued(request);
        record.state = InvocationState::Failed;
        record.termination = Some(Termination::Exit { code: 1 });
        record.summary = Some(InvocationSummary {
            success: false,
            headline: "failure ".repeat(4_000),
            ..InvocationSummary::default()
        });

        let text = RunResultBuilder::new(ResultEncoder::new(ResultEncoding::Text))
            .build_cli(&record)
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(value["truncated"], true);
        assert_eq!(value["available_views"], serde_json::json!([]));
        assert_eq!(value["more_available"], false);
        assert_eq!(
            value["rerun_hint"],
            "rerun with --no-agent-mode for unfiltered Bazel output"
        );
        assert!(text.len() <= 8 * 1024);
    }

    #[test]
    fn run_result_exposes_typed_program_outcome() {
        let mut record = InvocationRecord::queued(InvocationRequest::new(
            PathBuf::from("/workspace/example"),
            BazelCommand::Run,
            vec!["--config=dev".to_owned()],
        ));
        record.state = InvocationState::Failed;
        record.termination = Some(Termination::Exit { code: 7 });
        record.summary = Some(InvocationSummary {
            headline: "//cmd:example exited with code 7".to_owned(),
            ..Default::default()
        });
        record.run = Some(RunSummary {
            target: "//cmd:example".to_owned(),
            outcome: RunOutcome::ProgramFailed,
            program_exit_code: Some(7),
            output_excerpt: vec!["program output".to_owned()],
        });

        let encoded = RunResultBuilder::new(ResultEncoder::new(ResultEncoding::Structured))
            .build(ExecutionResult::new(&record, false))
            .unwrap();
        let value = encoded.result.structured_content.unwrap();

        assert_eq!(value["run"]["outcome"], "program_failed");
        assert_eq!(value["run"]["program_exit_code"], 7);
        assert_eq!(value["run"]["output_excerpt"][0], "program output");
    }

    #[test]
    fn every_encoding_packs_complete_inspection_items_to_the_exact_limit() {
        for encoding in [
            ResultEncoding::Text,
            ResultEncoding::Toon,
            ResultEncoding::Structured,
            ResultEncoding::Both,
        ] {
            let start_cursor = "start-cursor".to_owned();
            let result = InspectResult::new(
                None,
                InspectPayload::Log(vec![
                    format!("first-{}", "a".repeat(180)),
                    format!("second-{}", "b".repeat(180)),
                    format!("third-{}", "c".repeat(180)),
                ]),
                None,
                None,
                Some("after-all".to_owned()),
                true,
                vec![
                    "after-1".to_owned(),
                    "after-2".to_owned(),
                    "after-3".to_owned(),
                ],
            )
            .with_start_cursor(Some(start_cursor.clone()));
            let encoded = ResultEncoder::new(encoding)
                .encode_inspect(result, 512)
                .unwrap();
            assert!(encoded.visible_bytes <= 512, "encoding={encoding:?}");

            let value = match encoding {
                ResultEncoding::Text => {
                    let Some(ContentBlock::Text(text)) = encoded.result.content.first() else {
                        panic!("text encoding omitted content");
                    };
                    serde_json::from_str(&text.text).unwrap()
                }
                ResultEncoding::Toon => {
                    let Some(ContentBlock::Text(text)) = encoded.result.content.first() else {
                        panic!("TOON encoding omitted content");
                    };
                    toon_format::decode_default(&text.text).unwrap()
                }
                ResultEncoding::Structured | ResultEncoding::Both => {
                    encoded.result.structured_content.unwrap()
                }
            };
            assert_eq!(value["view"], InspectView::Log.as_str());
            let emitted = value["items"].as_array().unwrap().len();
            assert!(emitted < 3, "test fixture must exercise final packing");
            let expected_cursor = match emitted {
                0 => start_cursor.as_str(),
                1 => "after-1",
                2 => "after-2",
                _ => unreachable!(),
            };
            assert_eq!(value["next_cursor"], expected_cursor);
        }
    }
}
