use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::TranscriptMetrics;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdapterMetrics {
    pub(crate) adapter: String,
    pub(crate) scenario: String,
    pub(crate) cache_condition: String,
    pub sample: u32,
    pub(crate) bazel_wall_ms: u64,
    pub(crate) end_to_end_ms: u64,
    pub(crate) exit_code: Option<i32>,
    pub(crate) diagnostic_found: bool,
    pub(crate) raw_process_bytes: u64,
    pub(crate) transcript: TranscriptMetrics,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnvironmentMetadata {
    pub(crate) os: String,
    pub(crate) architecture: String,
    pub(crate) rustc: String,
    pub(crate) logical_cpus: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryStatistics {
    pub(crate) cache_condition: String,
    pub(crate) adapter: String,
    pub(crate) observations: usize,
    pub(crate) median_bazel_wall_ms: u64,
    pub(crate) p95_bazel_wall_ms: u64,
    pub(crate) median_context_tokens: u64,
    pub(crate) p95_context_tokens: u64,
    pub(crate) median_visible_bytes: u64,
    pub(crate) p95_visible_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Estimate {
    pub(crate) value: f64,
    pub(crate) ci95_lower: f64,
    pub(crate) ci95_upper: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaselineComparison {
    pub(crate) baseline_adapter: String,
    pub(crate) candidate_adapter: String,
    pub(crate) aggregate: BTreeMap<String, Estimate>,
    pub(crate) by_cache: BTreeMap<String, BTreeMap<String, Estimate>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub(crate) schema_version: u32,
    pub(crate) project: String,
    pub(crate) commit: String,
    pub(crate) bazel_version: String,
    pub(crate) tokenizer_crate_version: String,
    pub(crate) encoding: String,
    pub(crate) canonicalization_version: u32,
    pub(crate) adapter_order_seed: u64,
    pub(crate) environment: EnvironmentMetadata,
    pub samples: Vec<AdapterMetrics>,
    pub(crate) statistics: Vec<SummaryStatistics>,
    #[serde(default)]
    pub(crate) comparisons: Vec<BaselineComparison>,
    // Retained in schema v3 for consumers of schema v2 reports. These values
    // are the point estimates for shell-default versus bazel-mcp.
    pub(crate) aggregate_reduction_percent: BTreeMap<String, f64>,
    pub(crate) reduction_percent_by_cache: BTreeMap<String, BTreeMap<String, f64>>,
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
