use serde::{Deserialize, Serialize};
use tiktoken_rs::{CoreBPE, cl100k_base_singleton, o200k_base_singleton};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    System,
    Task,
    ToolSchema,
    ModelEvent,
    ToolCall,
    ToolResult,
    Progress,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEvent {
    pub(crate) sequence: u64,
    pub(crate) adapter: String,
    pub(crate) scenario: String,
    pub(crate) kind: TranscriptKind,
    pub(crate) role: String,
    pub(crate) model_visible: bool,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub(crate) canonicalization_version: u32,
    pub(crate) events: Vec<TranscriptEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TranscriptMetrics {
    encoding: String,
    pub(crate) visible_tool_tokens: u64,
    pub(crate) cumulative_context_tokens: u64,
    pub(crate) model_visible_bytes: u64,
    model_events: u64,
    tool_calls: u64,
    polling_calls: u64,
    #[serde(default)]
    protocol_polling_bytes: u64,
}

impl Transcript {
    pub(crate) fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut output = String::new();
        for event in &self.events {
            output.push_str(&serde_json::to_string(event)?);
            output.push('\n');
        }
        Ok(output)
    }

    pub(crate) fn measure(&self, encoding: &str) -> anyhow::Result<TranscriptMetrics> {
        let tokenizer = tokenizer(encoding)?;
        let mut prior_context = String::new();
        let mut visible_tool_tokens = 0_u64;
        let mut cumulative_context_tokens = 0_u64;
        let mut model_visible_bytes = 0_u64;
        let mut model_events = 0_u64;
        let mut tool_calls = 0_u64;
        let mut polling_calls = 0_u64;
        let mut protocol_polling_bytes = 0_u64;
        for event in &self.events {
            if event.kind == TranscriptKind::ModelEvent {
                model_events += 1;
                cumulative_context_tokens += token_count(tokenizer, &prior_context);
            }
            if event.kind == TranscriptKind::ToolCall {
                tool_calls += 1;
            }
            if event.kind == TranscriptKind::Progress {
                polling_calls += 1;
                if !event.model_visible {
                    protocol_polling_bytes =
                        protocol_polling_bytes.saturating_add(event.content.len() as u64);
                }
            }
            if event.model_visible {
                if matches!(
                    event.kind,
                    TranscriptKind::ToolResult | TranscriptKind::Progress
                ) {
                    visible_tool_tokens += token_count(tokenizer, &event.content);
                    model_visible_bytes += event.content.len() as u64;
                }
                prior_context.push_str(&event.role);
                prior_context.push(':');
                prior_context.push_str(&event.content);
                prior_context.push('\n');
            }
        }
        Ok(TranscriptMetrics {
            encoding: encoding.to_owned(),
            visible_tool_tokens,
            cumulative_context_tokens,
            model_visible_bytes,
            model_events,
            tool_calls,
            polling_calls,
            protocol_polling_bytes,
        })
    }
}

fn tokenizer(encoding: &str) -> anyhow::Result<&'static CoreBPE> {
    match encoding {
        "o200k_base" => Ok(o200k_base_singleton()),
        "cl100k_base" => Ok(cl100k_base_singleton()),
        other => anyhow::bail!("unsupported tokenizer encoding {other:?}"),
    }
}

fn token_count(tokenizer: &CoreBPE, value: &str) -> u64 {
    tokenizer.encode_with_special_tokens(value).len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_model_events_charge_prior_context_again() {
        let transcript = Transcript {
            canonicalization_version: 1,
            events: vec![
                TranscriptEvent {
                    sequence: 0,
                    adapter: "shell".into(),
                    scenario: "build".into(),
                    kind: TranscriptKind::Task,
                    role: "user".into(),
                    model_visible: true,
                    content: "build the target".into(),
                },
                TranscriptEvent {
                    sequence: 1,
                    adapter: "shell".into(),
                    scenario: "build".into(),
                    kind: TranscriptKind::ModelEvent,
                    role: "assistant".into(),
                    model_visible: true,
                    content: "".into(),
                },
                TranscriptEvent {
                    sequence: 2,
                    adapter: "shell".into(),
                    scenario: "build".into(),
                    kind: TranscriptKind::Progress,
                    role: "tool".into(),
                    model_visible: true,
                    content: "still running".into(),
                },
                TranscriptEvent {
                    sequence: 3,
                    adapter: "shell".into(),
                    scenario: "build".into(),
                    kind: TranscriptKind::ModelEvent,
                    role: "assistant".into(),
                    model_visible: true,
                    content: "".into(),
                },
            ],
        };
        let metrics = transcript.measure("o200k_base").unwrap();
        assert_eq!(metrics.model_events, 2);
        assert!(metrics.cumulative_context_tokens > metrics.visible_tool_tokens);
    }

    #[test]
    fn negotiated_task_transcripts_preserve_results_and_isolate_polling_bytes() {
        let transcripts = [
            include_str!("../resources/protocol/synchronous.jsonl"),
            include_str!("../resources/protocol/legacy-tasks.jsonl"),
            include_str!("../resources/protocol/tasks-extension.jsonl"),
        ];
        let mut final_results = Vec::new();
        for source in transcripts {
            let events = source
                .lines()
                .map(|line| serde_json::from_str::<TranscriptEvent>(line).unwrap())
                .collect::<Vec<_>>();
            let transcript = Transcript {
                canonicalization_version: 1,
                events,
            };
            let metrics = transcript.measure("o200k_base").unwrap();
            let result = transcript
                .events
                .iter()
                .rev()
                .find(|event| event.kind == TranscriptKind::ToolResult)
                .unwrap()
                .content
                .clone();
            final_results.push(result);
            if transcript.events[0].adapter == "synchronous" {
                assert_eq!(metrics.protocol_polling_bytes, 0);
            } else {
                assert!(metrics.protocol_polling_bytes > 0);
            }
        }
        assert!(final_results.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
