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
pub struct Estimate {
    pub value: f64,
    pub ci95_lower: f64,
    pub ci95_upper: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineComparison {
    pub baseline_adapter: String,
    pub candidate_adapter: String,
    pub aggregate: BTreeMap<String, Estimate>,
    pub by_cache: BTreeMap<String, BTreeMap<String, Estimate>>,
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
    #[serde(default)]
    pub comparisons: Vec<BaselineComparison>,
    // Retained in schema v3 for consumers of schema v2 reports. These values
    // are the point estimates for shell-default versus bazel-mcp.
    pub aggregate_reduction_percent: BTreeMap<String, f64>,
    pub reduction_percent_by_cache: BTreeMap<String, BTreeMap<String, f64>>,
}

impl BenchmarkReport {
    pub fn markdown(&self) -> String {
        let mut output = format!(
            "# Bazel MCP token integration\n\nProject: `{}` @ `{}`\n\nBazel: `{}`\nTokenizer: `tiktoken-rs {}` / `{}`\n\n",
            self.project,
            self.commit,
            self.bazel_version,
            self.tokenizer_crate_version,
            self.encoding
        );
        output.push_str("## Baseline comparisons\n\n");
        output
            .push_str("Intervals are deterministic paired bootstrap 95% confidence intervals.\n\n");
        output.push_str("| Baseline | Metric | Estimate | 95% CI |\n| --- | --- | ---: | ---: |\n");
        for comparison in &self.comparisons {
            for (metric, estimate) in &comparison.aggregate {
                output.push_str(&format!(
                    "| {} | {metric} | {:.2}% | {:.2}%–{:.2}% |\n",
                    comparison.baseline_adapter,
                    estimate.value,
                    estimate.ci95_lower,
                    estimate.ci95_upper,
                ));
            }
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
        output.push_str("\n## Comparisons by cache condition\n\n");
        output.push_str(
            "| Baseline | Cache | Metric | Estimate | 95% CI |\n| --- | --- | --- | ---: | ---: |\n",
        );
        for comparison in &self.comparisons {
            for (cache, metrics) in &comparison.by_cache {
                for (metric, estimate) in metrics {
                    output.push_str(&format!(
                        "| {} | {cache} | {metric} | {:.2}% | {:.2}%–{:.2}% |\n",
                        comparison.baseline_adapter,
                        estimate.value,
                        estimate.ci95_lower,
                        estimate.ci95_upper,
                    ));
                }
            }
        }
        output
    }
}
