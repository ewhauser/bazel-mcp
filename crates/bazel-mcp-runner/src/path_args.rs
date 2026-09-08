use std::path::Path;

pub(crate) fn bazel_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        windows_bazel_path(&path)
    } else {
        path.into_owned()
    }
}

fn windows_bazel_path(path: &str) -> String {
    // Canonical Windows filesystem paths use the verbatim prefix, but Bazel's
    // Java filesystem rejects it in file-valued command-line flags.
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    if let Some(drive) = path.strip_prefix(r"\\?\") {
        let bytes = drive.as_bytes();
        if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1..3] == *b":\\" {
            return drive.to_owned();
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_drive_path_is_compatible_with_bazels_java_parser() {
        assert_eq!(
            windows_bazel_path(r"\\?\C:\Users\runner\cache space\events.bep"),
            r"C:\Users\runner\cache space\events.bep"
        );
    }

    #[test]
    fn canonical_unc_path_keeps_its_network_root() {
        assert_eq!(
            windows_bazel_path(r"\\?\UNC\server\share\events.bep"),
            r"\\server\share\events.bep"
        );
    }

    #[test]
    fn ordinary_paths_and_device_names_are_unchanged() {
        for path in [
            r"C:\cache\events.bep",
            r"\\server\share\events.bep",
            r"\\?\Volume{abc}\file",
            "/tmp/events.bep",
        ] {
            assert_eq!(windows_bazel_path(path), path);
        }
    }
}
