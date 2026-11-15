//! Environment variable lookup for `TCHELVIS_*` variables.

/// Reads `TCHELVIS_<suffix>` from the process environment.
pub fn var(suffix: &str) -> Option<String> {
    var_with(suffix, |key| std::env::var(key).ok())
}

/// Reads `TCHELVIS_<suffix>` using the provided getter (for deterministic tests).
pub fn var_with(suffix: &str, get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let key = format!("TCHELVIS_{suffix}");
    get(&key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn var_with_reads_tchelvis_only() {
        let map_new: HashMap<&str, &str> = [("TCHELVIS_X", "new")].into_iter().collect();
        assert_eq!(
            var_with("X", |k| map_new.get(k).map(|s| (*s).to_string())),
            Some("new".to_string())
        );

        let map_old: HashMap<&str, &str> = [("COCKPIT_X", "old")].into_iter().collect();
        assert_eq!(
            var_with("X", |k| map_old.get(k).map(|s| (*s).to_string())),
            None
        );
    }
}
