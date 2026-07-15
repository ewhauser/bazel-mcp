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
    pub sequence: u64,
    pub adapter: String,
    pub scenario: String,
    pub kind: TranscriptKind,
    pub role: String,
    pub model_visible: bool,
    pub content: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub canonicalization_version: u32,
    pub events: Vec<TranscriptEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TranscriptMetrics {
    pub encoding: String,
    pub visible_tool_tokens: u64,
    pub cumulative_context_tokens: u64,
    pub model_visible_bytes: u64,
    pub model_events: u64,
    pub tool_calls: u64,
    pub polling_calls: u64,
}

impl Transcript {
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut output = String::new();
        for event in &self.events {
            output.push_str(&serde_json::to_string(event)?);
            output.push('\n');
        }
        Ok(output)
    }

    pub fn measure(&self, encoding: &str) -> anyhow::Result<TranscriptMetrics> {
        let tokenizer = tokenizer(encoding)?;
        let mut prior_context = String::new();
        let mut visible_tool_tokens = 0_u64;
        let mut cumulative_context_tokens = 0_u64;
        let mut model_visible_bytes = 0_u64;
        let mut model_events = 0_u64;
        let mut tool_calls = 0_u64;
        let mut polling_calls = 0_u64;
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
}
