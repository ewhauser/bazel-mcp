use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoverageFile {
    pub path: String,
    pub lines_found: u64,
    pub lines_hit: u64,
    pub coverage_percent: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub lines_found: u64,
    pub lines_hit: u64,
    pub coverage_percent: f64,
    pub files: Vec<CoverageFile>,
}
