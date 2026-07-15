use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::TranscriptMetrics;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdapterMetrics {
    pub adapter: String,
    pub scenario: String,
    pub cache_condition: String,
    pub sample: u32,
    pub bazel_wall_ms: u64,
    pub end_to_end_ms: u64,
    pub exit_code: Option<i32>,
    pub diagnostic_found: bool,
    pub raw_process_bytes: u64,
    pub transcript: TranscriptMetrics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentMetadata {
    pub os: String,
    pub architecture: String,
    pub rustc: String,
    pub logical_cpus: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryStatistics {
    pub cache_condition: String,
    pub adapter: String,
    pub observations: usize,
    pub median_bazel_wall_ms: u64,
    pub p95_bazel_wall_ms: u64,
    pub median_context_tokens: u64,
    pub p95_context_tokens: u64,
    pub median_visible_bytes: u64,
    pub p95_visible_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: u32,
    pub project: String,
    pub commit: String,
    pub bazel_version: String,
    pub tokenizer_crate_version: String,
    pub encoding: String,
    pub canonicalization_version: u32,
    pub adapter_order_seed: u64,
    pub environment: EnvironmentMetadata,
    pub samples: Vec<AdapterMetrics>,
    pub statistics: Vec<SummaryStatistics>,
    pub aggregate_reduction_percent: BTreeMap<String, f64>,
    pub reduction_percent_by_cache: BTreeMap<String, BTreeMap<String, f64>>,
}

impl BenchmarkReport {
    pub fn markdown(&self) -> String {
        let mut output = format!(
            "# Bazel MCP token integration\n\nProject: `{}` @ `{}`\n\nBazel: `{}`  \nTokenizer: `tiktoken-rs {}` / `{}`\n\n",
            self.project,
            self.commit,
            self.bazel_version,
            self.tokenizer_crate_version,
            self.encoding
        );
        output.push_str("| Metric | Reduction |\n| --- | ---: |\n");
        for (metric, value) in &self.aggregate_reduction_percent {
            output.push_str(&format!("| {metric} | {value:.2}% |\n"));
        }
        output.push_str("\n| Cache | Adapter | N | Median Bazel ms | p95 Bazel ms | Median context tokens | p95 context tokens | Median visible bytes | p95 visible bytes |\n");
        output.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for statistic in &self.statistics {
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                statistic.cache_condition,
                statistic.adapter,
                statistic.observations,
                statistic.median_bazel_wall_ms,
                statistic.p95_bazel_wall_ms,
                statistic.median_context_tokens,
                statistic.p95_context_tokens,
                statistic.median_visible_bytes,
                statistic.p95_visible_bytes,
            ));
        }
        output.push_str("\n## Reduction by cache condition\n\n");
        output.push_str("| Cache | Metric | Reduction |\n| --- | --- | ---: |\n");
        for (cache, metrics) in &self.reduction_percent_by_cache {
            for (metric, value) in metrics {
                output.push_str(&format!("| {cache} | {metric} | {value:.2}% |\n"));
            }
        }
        output
    }
}
