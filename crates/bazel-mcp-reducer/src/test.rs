use bazel_mcp_types::{TestCase, TestStatus};
use quick_xml::de::from_str;
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TestXmlError {
    #[error(transparent)]
    Xml(#[from] quick_xml::DeError),
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(rename = "testsuite", default)]
    suites: Vec<Suite>,
    #[serde(rename = "testcase", default)]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Suite {
    #[serde(rename = "testcase", default)]
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "@time")]
    time: Option<f64>,
    failure: Option<Message>,
    error: Option<Message>,
    skipped: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "@message")]
    message: Option<String>,
    #[serde(rename = "$text")]
    text: Option<String>,
}

pub fn parse_test_xml(input: &str) -> Result<Vec<TestCase>, TestXmlError> {
    let document: Document = from_str(input)?;
    Ok(document
        .cases
        .into_iter()
        .chain(document.suites.into_iter().flat_map(|suite| suite.cases))
        .map(|case| {
            let (status, detail) = if let Some(message) = case.failure.or(case.error) {
                (TestStatus::Failed, message.message.or(message.text))
            } else if let Some(message) = case.skipped {
                (TestStatus::Skipped, message.message.or(message.text))
            } else {
                (TestStatus::Passed, None)
            };
            TestCase {
                name: case.name,
                status,
                duration_ms: case.time.and_then(duration_ms),
                message: detail,
            }
        })
        .collect())
}

fn duration_ms(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let milliseconds = seconds * 1_000.0;
    if milliseconds >= u64::MAX as f64 {
        Some(u64::MAX)
    } else {
        Some(milliseconds as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_and_negative_test_durations() {
        assert_eq!(duration_ms(-1.0), None);
        assert_eq!(duration_ms(f64::NAN), None);
        assert_eq!(duration_ms(f64::INFINITY), None);
        assert_eq!(duration_ms(1.25), Some(1_250));
        assert_eq!(duration_ms(f64::MAX), Some(u64::MAX));
    }

    #[test]
    fn parses_direct_testsuite_root() {
        let cases = parse_test_xml(
            r#"<testsuite><testcase name="one" time="0.25"><failure message="bad"/></testcase></testsuite>"#,
        )
        .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].name, "one");
        assert_eq!(cases[0].status, TestStatus::Failed);
    }

    #[test]
    fn parses_testsuites_wrapper() {
        let cases = parse_test_xml(
            r#"<testsuites><testsuite><testcase name="one" time="0.25"><failure message="bad"/></testcase></testsuite><testsuite><testcase name="two" time="0.1"/></testsuite></testsuites>"#,
        )
        .unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].status, TestStatus::Failed);
        assert_eq!(cases[1].status, TestStatus::Passed);
    }
}
