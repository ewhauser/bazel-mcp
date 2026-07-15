#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: (&str, &str)| {
    let (pattern, value) = data;
    if pattern.len() < 256 {
        if let Ok(redactor) = bazel_mcp_policy::Redactor::new(&[pattern.to_owned()]) {
            let _ = redactor.redact(value);
        }
    }
});
