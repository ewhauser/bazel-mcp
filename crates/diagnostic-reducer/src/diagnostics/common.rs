pub(crate) fn bounded_text(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut boundary = maximum_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &value[..boundary])
}

pub(super) fn split_u32_prefix(value: &str) -> Option<(u32, &str)> {
    let (number, remainder) = value.split_once(':')?;
    let number = number.trim();
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| number.parse::<u32>().ok().map(|number| (number, remainder)))
        .flatten()
}

pub(super) fn normalize_path(path: &str) -> String {
    let path = path.trim_matches('"').replace('\\', "/");
    path.strip_prefix("./").unwrap_or(&path).to_owned()
}
