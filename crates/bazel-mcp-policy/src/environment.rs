use std::collections::BTreeMap;

use crate::PolicyConfig;

const ALWAYS_ALLOWED: &[&str] = &["HOME", "PATH", "TMPDIR", "TEMP", "TMP", "USER"];

#[must_use]
pub fn filtered_environment(config: &PolicyConfig) -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| {
            ALWAYS_ALLOWED.contains(&name.as_str()) || config.environment_allowlist.contains(name)
        })
        .collect()
}
