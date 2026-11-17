use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{table, value, DocumentMut, Item};

fn edit_supervisor_toml(
    path: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> Result<()>,
) -> Result<()> {
    let mut doc = if path.exists() {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        raw.parse::<DocumentMut>()
            .with_context(|| format!("Invalid TOML in {}", path.display()))?
    } else {
        DocumentMut::new()
    };

    edit(&mut doc)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, doc.to_string())
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

fn ensure_llm_container_table(doc: &mut DocumentMut) -> Result<&mut toml_edit::Table> {
    if !doc.contains_key("llm_container") {
        doc["llm_container"] = table();
    }
    match &mut doc["llm_container"] {
        Item::Table(t) => Ok(t),
        _ => bail!("[llm_container] in supervisor.toml is not a table"),
    }
}

/// Persists `llm_container.model_id` (and top-level `ollama_model` when `ollama_name` is `Some`).
pub fn write_model_selection(path: &Path, model_id: &str, ollama_name: Option<&str>) -> Result<()> {
    edit_supervisor_toml(path, |doc| {
        if let Some(name) = ollama_name {
            doc["ollama_model"] = value(name);
        }
        let tbl = ensure_llm_container_table(doc)?;
        tbl["model_id"] = value(model_id);
        Ok(())
    })
}

/// Persists `llm_container.model_filter`.
pub fn write_model_filter(path: &Path, filter: &str) -> Result<()> {
    edit_supervisor_toml(path, |doc| {
        let tbl = ensure_llm_container_table(doc)?;
        tbl["model_filter"] = value(filter);
        Ok(())
    })
}

/// Removes only `llm_container.model_id` from `supervisor.toml`.
pub fn clear_model_selection(path: &Path) -> Result<()> {
    edit_supervisor_toml(path, |doc| {
        if let Some(Item::Table(tbl)) = doc.get_mut("llm_container") {
            tbl.remove("model_id");
        }
        Ok(())
    })
}

/// Persists `llm_container.hidden_models`, or removes the key when `ids` is empty.
pub fn write_hidden_models(path: &Path, ids: &[String]) -> Result<()> {
    edit_supervisor_toml(path, |doc| {
        if ids.is_empty() {
            if let Some(Item::Table(tbl)) = doc.get_mut("llm_container") {
                tbl.remove("hidden_models");
            }
            return Ok(());
        }
        let tbl = ensure_llm_container_table(doc)?;
        let mut arr = toml_edit::Array::new();
        for id in ids {
            arr.push(id.as_str());
        }
        tbl["hidden_models"] = value(arr);
        Ok(())
    })
}

fn ensure_fallback_table(doc: &mut DocumentMut) -> Result<&mut toml_edit::Table> {
    if !doc.contains_key("fallback") {
        doc["fallback"] = table();
    }
    match &mut doc["fallback"] {
        Item::Table(t) => Ok(t),
        _ => bail!("[fallback] in supervisor.toml is not a table"),
    }
}

/// Persists `[fallback] host` and/or `model` in `supervisor.toml` without writing any API key.
pub fn write_fallback(path: &Path, host: Option<&str>, model: Option<&str>) -> Result<()> {
    edit_supervisor_toml(path, |doc| {
        let tbl = ensure_fallback_table(doc)?;
        if let Some(h) = host {
            tbl["host"] = value(h.trim());
        }
        if let Some(m) = model {
            tbl["model"] = value(m.trim());
        }
        Ok(())
    })
}

/// Removes the `[fallback]` table from `supervisor.toml`.
pub fn clear_fallback(path: &Path) -> Result<()> {
    edit_supervisor_toml(path, |doc| {
        doc.remove("fallback");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::config::SupervisorConfig;
    use std::fs;

    #[test]
    fn preserves_comments_and_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        let original =
            "# keep me\npoll_interval_secs = 30\n\n[llm_container]\nport = 11500 # mine\n";
        fs::write(&path, original).unwrap();

        write_model_selection(&path, "file:/m/a.gguf", None).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("# keep me"));
        assert!(updated.contains("poll_interval_secs = 30"));
        assert!(updated.contains("port = 11500 # mine"));
        assert!(updated.contains("model_id = \"file:/m/a.gguf\""));
    }

    #[test]
    fn sets_ollama_model_for_ollama_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        fs::write(&path, "ollama_model = \"old:1b\"\n").unwrap();

        write_model_selection(&path, "ollama:qwen2.5:3b", Some("qwen2.5:3b")).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let parsed: SupervisorConfig = toml::from_str(&updated).unwrap();
        assert_eq!(parsed.ollama_model, "qwen2.5:3b");
        assert_eq!(
            parsed.llm_container.model_id.as_deref(),
            Some("ollama:qwen2.5:3b")
        );
    }

    #[test]
    fn creates_missing_file_and_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/supervisor.toml");
        write_model_selection(&path, "file:/m/b.gguf", None).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let parsed: SupervisorConfig = toml::from_str(&raw).unwrap();
        assert_eq!(
            parsed.llm_container.model_id.as_deref(),
            Some("file:/m/b.gguf")
        );
    }

    #[test]
    fn clear_removes_only_model_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        fs::write(
            &path,
            "ollama_model = \"qwen2.5:3b\"\n\n[llm_container]\nmodel_id = \"ollama:qwen2.5:3b\"\nmodel_filter = \"2b\"\n",
        )
        .unwrap();

        clear_model_selection(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let parsed: SupervisorConfig = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.llm_container.model_id, None);
        assert_eq!(parsed.llm_container.model_filter, "2b");
        assert_eq!(parsed.ollama_model, "qwen2.5:3b");
    }

    #[test]
    fn write_filter_sets_model_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        write_model_filter(&path, "8b").unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let parsed: SupervisorConfig = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.llm_container.model_filter, "8b");
    }

    #[test]
    fn write_refuses_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        let broken = b"poll_interval_secs = = 3\n";
        fs::write(&path, broken).unwrap();

        assert!(write_model_selection(&path, "file:/m/a.gguf", None).is_err());
        assert_eq!(fs::read(&path).unwrap(), broken);
    }

    #[test]
    fn write_hidden_models_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        fs::write(
            &path,
            "# keep\npoll_interval_secs = 25\n\n[llm_container]\nmodel_filter = \"8b\"\n",
        )
        .unwrap();

        let ids = vec!["a".to_string(), "b".to_string()];
        write_hidden_models(&path, &ids).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# keep"));
        let parsed: SupervisorConfig = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.poll_interval_secs, 25);
        assert_eq!(parsed.llm_container.model_filter, "8b");
        assert_eq!(parsed.llm_container.hidden_models, ids);

        write_hidden_models(&path, &[]).unwrap();
        let cleared_raw = fs::read_to_string(&path).unwrap();
        assert!(!cleared_raw.contains("hidden_models"));
        let cleared_parsed: SupervisorConfig = toml::from_str(&cleared_raw).unwrap();
        assert!(cleared_parsed.llm_container.hidden_models.is_empty());
        assert_eq!(cleared_parsed.llm_container.model_filter, "8b");
        assert_eq!(cleared_parsed.poll_interval_secs, 25);
    }

    #[test]
    fn write_and_clear_fallback_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("supervisor.toml");
        fs::write(
            &path,
            "# keep\npoll_interval_secs = 25\n\n[llm_container]\nport = 11435\n",
        )
        .unwrap();

        write_fallback(&path, Some("https://openrouter.ai/api"), Some("m1")).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# keep"));
        assert!(raw.contains("poll_interval_secs = 25"));
        assert!(raw.contains("port = 11435"));
        let parsed: SupervisorConfig = toml::from_str(&raw).unwrap();
        assert_eq!(parsed.fallback.host, "https://openrouter.ai/api");
        assert_eq!(parsed.fallback.model, "m1");

        write_fallback(&path, None, Some("m2")).unwrap();
        let raw2 = fs::read_to_string(&path).unwrap();
        let parsed2: SupervisorConfig = toml::from_str(&raw2).unwrap();
        assert_eq!(parsed2.fallback.host, "https://openrouter.ai/api");
        assert_eq!(parsed2.fallback.model, "m2");

        clear_fallback(&path).unwrap();
        let raw3 = fs::read_to_string(&path).unwrap();
        assert!(!raw3.contains("[fallback]"));
        let parsed3: SupervisorConfig = toml::from_str(&raw3).unwrap();
        assert_eq!(parsed3.fallback, Default::default());
        assert_eq!(parsed3.poll_interval_secs, 25);
    }
}
