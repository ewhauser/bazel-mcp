use std::collections::BTreeMap;

use crate::PolicyConfig;

const ALWAYS_ALLOWED: &[&str] = &["HOME", "PATH", "TMPDIR", "TEMP", "TMP", "USER"];

#[must_use]
pub fn filtered_environment(config: &PolicyConfig) -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| is_allowed(name, config, cfg!(windows)))
        .collect()
}

fn is_allowed(name: &str, config: &PolicyConfig, windows: bool) -> bool {
    if windows {
        // Windows environment names are case-insensitive. SystemRoot is needed
        // by Windows runtime services, including Winsock name resolution.
        name.eq_ignore_ascii_case("SystemRoot")
            || ALWAYS_ALLOWED
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
            || config
                .environment_allowlist
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
    } else {
        ALWAYS_ALLOWED.contains(&name) || config.environment_allowlist.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_preserves_system_root_and_case_insensitive_names() {
        let mut config = PolicyConfig::default();
        config
            .environment_allowlist
            .insert("BAZELISK_HOME".to_owned());
        for name in [
            "SystemRoot",
            "SYSTEMROOT",
            "systemroot",
            "Path",
            "TEMP",
            "Bazelisk_Home",
        ] {
            assert!(is_allowed(name, &config, true), "missing {name}");
        }
        for name in ["GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "UNLISTED"] {
            assert!(!is_allowed(name, &config, true), "leaked {name}");
        }
    }

    #[test]
    fn unix_environment_names_remain_case_sensitive() {
        let mut config = PolicyConfig::default();
        config
            .environment_allowlist
            .insert("BAZELISK_HOME".to_owned());
        for name in ["PATH", "HOME", "BAZELISK_HOME"] {
            assert!(is_allowed(name, &config, false));
        }
        for name in [
            "Path",
            "home",
            "Bazelisk_Home",
            "SystemRoot",
            "GITHUB_TOKEN",
        ] {
            assert!(!is_allowed(name, &config, false));
        }
    }
}
