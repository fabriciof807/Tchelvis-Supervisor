//! Build-time version and channel metadata.

/// Formats `<pkg> (<channel> [<commit>])`, defaulting an absent channel to `dev`.
pub fn format_version(pkg: &str, channel: Option<&str>, commit: Option<&str>) -> String {
    let ch = channel
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("dev");
    match commit.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sha) => format!("{pkg} ({ch} {sha})"),
        None => format!("{pkg} ({ch})"),
    }
}

/// Returns the compile-time `TCHELVIS_CHANNEL`, if set and non-empty.
pub fn build_channel() -> Option<&'static str> {
    option_env!("TCHELVIS_CHANNEL").filter(|s| !s.is_empty())
}

/// Returns the compile-time `TCHELVIS_COMMIT`, if set and non-empty.
pub fn build_commit() -> Option<&'static str> {
    option_env!("TCHELVIS_COMMIT").filter(|s| !s.is_empty())
}

/// Returns a `'static` version string suitable for clap's `#[command(version = ...)]`.
pub fn long_version() -> &'static str {
    let formatted = format_version(env!("CARGO_PKG_VERSION"), build_channel(), build_commit());
    Box::leak(formatted.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_version_cases() {
        assert_eq!(
            format_version("0.2.0", Some("nightly"), Some("a1b2c3d")),
            "0.2.0 (nightly a1b2c3d)"
        );
        assert_eq!(
            format_version("0.2.0", Some("stable"), None),
            "0.2.0 (stable)"
        );
        assert_eq!(format_version("0.2.0", None, None), "0.2.0 (dev)");
    }
}
