use std::{collections::BTreeMap, io::BufRead};

use bazel_mcp_types::{CoverageFile, CoverageSummary};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoverageError {
    #[error("invalid LCOV integer {value:?}")]
    InvalidInteger { value: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn parse_lcov(input: &str) -> Result<CoverageSummary, CoverageError> {
    parse_lcov_reader(std::io::Cursor::new(input.as_bytes()))
}

pub fn parse_lcov_reader(input: impl BufRead) -> Result<CoverageSummary, CoverageError> {
    let mut current = None::<String>;
    let mut files = BTreeMap::<String, (u64, u64)>::new();
    for line in input.lines() {
        let line = line?;
        if let Some(path) = line.strip_prefix("SF:") {
            current = Some(bounded_path(path));
        } else if let Some(data) = line.strip_prefix("DA:") {
            let mut fields = data.split(',');
            let _line = parse_u64(fields.next().unwrap_or_default())?;
            let hits = parse_u64(fields.next().unwrap_or_default())?;
            if let Some(path) = &current {
                let totals = files.entry(path.clone()).or_default();
                totals.0 = totals.0.saturating_add(1);
                totals.1 = totals.1.saturating_add(u64::from(hits > 0));
            }
        }
    }
    let lines_found = files
        .values()
        .fold(0_u64, |total, (found, _)| total.saturating_add(*found));
    let lines_hit = files
        .values()
        .fold(0_u64, |total, (_, hit)| total.saturating_add(*hit));
    let files = files
        .into_iter()
        .map(|(path, (found, hit))| CoverageFile {
            path,
            lines_found: found,
            lines_hit: hit,
            coverage_percent: percent(hit, found),
        })
        .collect();
    Ok(CoverageSummary {
        lines_found,
        lines_hit,
        coverage_percent: percent(lines_hit, lines_found),
        files,
    })
}

fn bounded_path(value: &str) -> String {
    if value.len() <= 1_000 {
        return value.to_owned();
    }
    let mut boundary = 1_000;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

fn parse_u64(value: &str) -> Result<u64, CoverageError> {
    value.parse().map_err(|_| CoverageError::InvalidInteger {
        value: value.to_owned(),
    })
}

fn percent(hit: u64, found: u64) -> f64 {
    if found == 0 {
        0.0
    } else {
        hit as f64 * 100.0 / found as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_line_coverage() {
        let report = parse_lcov("SF:a.rs\nDA:1,1\nDA:2,0\nend_of_record\n").unwrap();
        assert_eq!(report.lines_found, 2);
        assert_eq!(report.lines_hit, 1);
        assert_eq!(report.coverage_percent, 50.0);
    }
}
