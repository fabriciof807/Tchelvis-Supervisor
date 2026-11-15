use super::container::ModelId;
use super::models::{shard_parts, LocalModel};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeletePlan {
    Ollama {
        name: String,
    },
    Files {
        remove: Vec<PathBuf>,
        prune_dir: Option<PathBuf>,
        bytes: u64,
    },
}

fn is_gguf(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}

fn hf_repo_dir(blob_path: &Path) -> Option<PathBuf> {
    let blobs_dir = blob_path.parent()?;
    if blobs_dir.file_name()?.to_str()? != "blobs" {
        return None;
    }
    let repo_dir = blobs_dir.parent()?;
    let repo_name = repo_dir.file_name()?.to_str()?;
    if !repo_name.starts_with("models--") {
        return None;
    }
    if !repo_dir.join("snapshots").is_dir() {
        return None;
    }
    Some(repo_dir.to_path_buf())
}

fn walk_snapshot_entries(
    dir: &Path,
    symlinks: &mut Vec<(PathBuf, Option<PathBuf>)>,
    regular_ggufs: &mut Vec<PathBuf>,
) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_symlink() {
            let target = fs::canonicalize(&path).ok();
            symlinks.push((path, target));
        } else if ft.is_dir() {
            walk_snapshot_entries(&path, symlinks, regular_ggufs);
        } else if ft.is_file() && is_gguf(&path) {
            regular_ggufs.push(path);
        }
    }
}

pub fn plan_delete(model: &LocalModel) -> Result<DeletePlan> {
    if let Ok(parsed) = ModelId::parse(&model.id) {
        match parsed {
            ModelId::Ollama(name) => return Ok(DeletePlan::Ollama { name }),
            ModelId::Hf(repo) => bail!("cannot delete remote hf model: {repo}"),
            ModelId::File(_) => {}
        }
    }

    let raw_path = if !model.path.as_os_str().is_empty() {
        model.path.clone()
    } else if let Ok(ModelId::File(p)) = ModelId::parse(&model.id) {
        p
    } else {
        bail!("model path missing for {}", model.id);
    };
    let canon_path = fs::canonicalize(&raw_path).unwrap_or(raw_path);
    // HF blobs have no extension; their snapshot symlinks must still be `.gguf`.
    if hf_repo_dir(&canon_path).is_none() && !is_gguf(&canon_path) {
        bail!(
            "refusing to delete {}: not a .gguf file",
            canon_path.display()
        );
    }

    if let Some(repo_dir) = hf_repo_dir(&canon_path) {
        let snapshots_dir = repo_dir.join("snapshots");
        let mut all_symlinks = Vec::new();
        let mut regular_ggufs = Vec::new();
        walk_snapshot_entries(&snapshots_dir, &mut all_symlinks, &mut regular_ggufs);

        let mut matching_symlinks: Vec<PathBuf> = all_symlinks
            .iter()
            .filter_map(|(sym, target)| {
                if target.as_ref() == Some(&canon_path) {
                    Some(sym.clone())
                } else {
                    None
                }
            })
            .collect();
        matching_symlinks.sort();

        // If multiple snapshot revisions share the same blob, pick one revision's symlink
        // (preferring matching display_name stem if present) so other revisions keep the blob.
        let mut selected_symlinks = BTreeSet::new();
        if let Some(primary) = matching_symlinks
            .iter()
            .find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s == model.display_name)
                    .unwrap_or(false)
            })
            .or_else(|| matching_symlinks.first())
            .cloned()
        {
            selected_symlinks.insert(primary.clone());
            if let (Some(parent), Some(fname)) = (
                primary.parent(),
                primary.file_name().and_then(|n| n.to_str()),
            ) {
                if let Some((base, total)) = shard_parts(fname) {
                    for idx in 1..=total.min(1000) {
                        let sib = parent.join(format!("{base}-{idx:05}-of-{total:05}.gguf"));
                        if fs::symlink_metadata(&sib)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false)
                        {
                            selected_symlinks.insert(sib);
                        }
                    }
                }
            }
        }

        let mut candidate_blobs = BTreeSet::new();
        if selected_symlinks.is_empty() {
            if canon_path.is_file() {
                candidate_blobs.insert(canon_path.clone());
            }
        } else {
            for sym in &selected_symlinks {
                if let Ok(target) = fs::canonicalize(sym) {
                    if target.is_file() {
                        candidate_blobs.insert(target);
                    }
                }
            }
        }

        let still_referenced: BTreeSet<PathBuf> = all_symlinks
            .iter()
            .filter(|(sym, _)| !selected_symlinks.contains(sym))
            .filter_map(|(_, target)| target.clone())
            .collect();

        let blobs_to_remove: Vec<PathBuf> = candidate_blobs
            .into_iter()
            .filter(|b| !still_referenced.contains(b))
            .collect();

        let mut bytes = 0u64;
        for blob in &blobs_to_remove {
            if let Ok(meta) = fs::metadata(blob) {
                if meta.is_file() {
                    bytes = bytes.saturating_add(meta.len());
                }
            }
        }

        let mut remove: Vec<PathBuf> = selected_symlinks.iter().cloned().collect();
        remove.extend(blobs_to_remove);

        let other_gguf_exists = all_symlinks
            .iter()
            .any(|(sym, _)| !selected_symlinks.contains(sym) && is_gguf(sym))
            || !regular_ggufs.is_empty();
        let prune_dir = if !other_gguf_exists {
            Some(repo_dir)
        } else {
            None
        };

        return Ok(DeletePlan::Files {
            remove,
            prune_dir,
            bytes,
        });
    }

    // Flat file (or non-HF layout)
    let mut remove = Vec::new();
    let mut bytes = 0u64;
    let file_name = canon_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    if let (Some(parent), Some((base, total))) = (canon_path.parent(), shard_parts(&file_name)) {
        for idx in 1..=total.min(1000) {
            let sib = parent.join(format!("{base}-{idx:05}-of-{total:05}.gguf"));
            if let Ok(meta) = fs::symlink_metadata(&sib) {
                if !meta.file_type().is_dir() {
                    if meta.file_type().is_file() {
                        bytes = bytes.saturating_add(meta.len());
                    }
                    remove.push(sib);
                }
            }
        }
    } else {
        let meta = fs::symlink_metadata(&canon_path)
            .with_context(|| format!("Model file not found: {}", canon_path.display()))?;
        if meta.file_type().is_file() {
            bytes = meta.len();
        }
        remove.push(canon_path);
    }

    if remove.is_empty() {
        bail!("Model file not found: {}", model.path.display());
    }

    Ok(DeletePlan::Files {
        remove,
        prune_dir: None,
        bytes,
    })
}

#[derive(Serialize, Debug, PartialEq)]
pub struct DeleteOutcome {
    pub id: String,
    pub plan: DeletePlan,
    pub deleted: bool,
}

pub trait OllamaRemover {
    fn remove(&self, name: &str) -> Result<()>;
}

pub struct SystemOllamaRemover {
    pub host: String,
}

impl OllamaRemover for SystemOllamaRemover {
    fn remove(&self, name: &str) -> Result<()> {
        if super::container::binary_on_path("ollama") {
            let out = std::process::Command::new("ollama")
                .args(["rm", name])
                .stdin(std::process::Stdio::null())
                .output()
                .with_context(|| format!("failed to execute `ollama rm {name}`"))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let detail = if !stderr.is_empty() { stderr } else { stdout };
                bail!("`ollama rm {name}` failed ({}): {detail}", out.status);
            }
            return Ok(());
        }

        let url = format!("{}/api/delete", self.host.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        match agent
            .delete(&url)
            .send_json(serde_json::json!({ "model": name }))
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                bail!("Ollama DELETE {url} failed ({code}): {body}");
            }
            Err(ureq::Error::Transport(err)) => {
                bail!("Ollama DELETE {url} failed: {err}");
            }
        }
    }
}

fn has_any_gguf_under_dir(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_symlink() || ft.is_file() {
            if is_gguf(&path) {
                return true;
            }
        } else if ft.is_dir() && has_any_gguf_under_dir(&path) {
            return true;
        }
    }
    false
}

pub fn execute_plan(plan: &DeletePlan, ollama: &dyn OllamaRemover) -> Result<()> {
    match plan {
        DeletePlan::Ollama { name } => ollama.remove(name),
        DeletePlan::Files {
            remove, prune_dir, ..
        } => {
            for path in remove {
                fs::remove_file(path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
            if let Some(repo) = prune_dir {
                let is_hf_repo = repo
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("models--"))
                    .unwrap_or(false);
                if is_hf_repo && !has_any_gguf_under_dir(&repo.join("snapshots")) {
                    fs::remove_dir_all(repo)
                        .with_context(|| format!("failed to prune {}", repo.display()))?;
                }
            }
            Ok(())
        }
    }
}

pub fn protected_reason(
    id: &str,
    effective: Option<&str>,
    fallback: Option<&str>,
    running: Option<&str>,
) -> Option<String> {
    if effective.map(str::trim).filter(|s| !s.is_empty()) == Some(id) {
        return Some("in use by the supervisor".to_string());
    }
    if fallback.map(str::trim).filter(|s| !s.is_empty()) == Some(id) {
        return Some("configured as fallback model".to_string());
    }
    if running.map(str::trim).filter(|s| !s.is_empty()) == Some(id) {
        return Some("loaded in the running container".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::models::ModelKind;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::path::Path;

    fn make_file_model(path: &Path) -> LocalModel {
        let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        LocalModel {
            id: format!("file:{}", canon.display()),
            display_name: "m".to_string(),
            source: "Folder".to_string(),
            secondary_label: None,
            path: canon,
            size_bytes: 0,
            params: None,
            params_label: "?".to_string(),
            quant: None,
            kind: ModelKind::Chat,
            in_use: false,
            hidden: false,
        }
    }

    #[test]
    fn non_gguf_file_is_refused() {
        // Defense in depth: never delete a file that is not a model, whatever the id says
        // (e.g. a `file:` id pointing at an Ollama blob or a config file).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sha256-abc");
        fs::write(&file, b"blob").unwrap();
        let model = make_file_model(&file);
        assert!(plan_delete(&model).is_err());
        assert!(file.exists());
    }

    #[test]
    fn flat_file_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tiny.gguf");
        fs::write(&file, vec![1u8; 123]).unwrap();
        let model = make_file_model(&file);

        let plan = plan_delete(&model).unwrap();
        assert_eq!(
            plan,
            DeletePlan::Files {
                remove: vec![model.path.clone()],
                prune_dir: None,
                bytes: 123,
            }
        );
    }

    #[test]
    fn flat_shards_all_removed() {
        let dir = tempfile::tempdir().unwrap();
        let s1 = dir.path().join("big-00001-of-00003.gguf");
        let s2 = dir.path().join("big-00002-of-00003.gguf");
        let s3 = dir.path().join("big-00003-of-00003.gguf");
        fs::write(&s1, vec![1u8; 100]).unwrap();
        fs::write(&s2, vec![2u8; 200]).unwrap();
        fs::write(&s3, vec![3u8; 300]).unwrap();

        let model = make_file_model(&s1);
        let plan = plan_delete(&model).unwrap();
        match plan {
            DeletePlan::Files {
                remove,
                prune_dir,
                bytes,
            } => {
                assert_eq!(remove.len(), 3);
                assert!(remove.contains(&fs::canonicalize(&s1).unwrap()));
                assert!(remove.contains(&fs::canonicalize(&s2).unwrap()));
                assert!(remove.contains(&fs::canonicalize(&s3).unwrap()));
                assert_eq!(prune_dir, None);
                assert_eq!(bytes, 600);
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hf_single_model_prunes_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path())
            .unwrap()
            .join("models--org--repo");
        let blob = repo.join("blobs/sha256-aaa");
        let snap = repo.join("snapshots/rev1/model.gguf");
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        fs::write(&blob, vec![9u8; 512]).unwrap();
        symlink(&blob, &snap).unwrap();

        let mut model = make_file_model(&blob);
        model.source = "HF cache".to_string();

        let plan = plan_delete(&model).unwrap();
        match plan {
            DeletePlan::Files {
                remove,
                prune_dir,
                bytes,
            } => {
                assert_eq!(remove.len(), 2);
                assert!(remove.contains(&snap));
                assert!(remove.contains(&blob));
                assert_eq!(prune_dir, Some(repo));
                assert_eq!(bytes, 512);
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hf_shared_blob_kept() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path())
            .unwrap()
            .join("models--org--repo");
        let blob = repo.join("blobs/sha256-shared");
        let snap1 = repo.join("snapshots/rev1/model.gguf");
        let snap2 = repo.join("snapshots/rev2/model.gguf");
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::create_dir_all(snap1.parent().unwrap()).unwrap();
        fs::create_dir_all(snap2.parent().unwrap()).unwrap();
        fs::write(&blob, vec![7u8; 400]).unwrap();
        symlink(&blob, &snap1).unwrap();
        symlink(&blob, &snap2).unwrap();

        let mut model = make_file_model(&blob);
        model.source = "HF cache".to_string();
        let plan = plan_delete(&model).unwrap();
        match plan {
            DeletePlan::Files {
                remove,
                prune_dir,
                bytes,
            } => {
                assert_eq!(remove.len(), 1);
                assert!(remove[0] == snap1 || remove[0] == snap2);
                assert!(!remove.contains(&blob));
                assert_eq!(prune_dir, None);
                assert_eq!(bytes, 0);
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hf_shards_all_removed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path())
            .unwrap()
            .join("models--org--repo");
        let b1 = repo.join("blobs/b1");
        let b2 = repo.join("blobs/b2");
        let s1 = repo.join("snapshots/rev1/big-00001-of-00002.gguf");
        let s2 = repo.join("snapshots/rev1/big-00002-of-00002.gguf");
        fs::create_dir_all(b1.parent().unwrap()).unwrap();
        fs::create_dir_all(s1.parent().unwrap()).unwrap();
        fs::write(&b1, vec![1u8; 250]).unwrap();
        fs::write(&b2, vec![2u8; 350]).unwrap();
        symlink(&b1, &s1).unwrap();
        symlink(&b2, &s2).unwrap();

        let mut model = make_file_model(&b1);
        model.source = "HF cache".to_string();

        let plan = plan_delete(&model).unwrap();
        match plan {
            DeletePlan::Files {
                remove,
                prune_dir,
                bytes,
            } => {
                assert_eq!(remove.len(), 4);
                assert!(remove.contains(&s1));
                assert!(remove.contains(&s2));
                assert!(remove.contains(&b1));
                assert!(remove.contains(&b2));
                assert_eq!(prune_dir, Some(repo));
                assert_eq!(bytes, 600);
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn hf_other_model_keeps_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path())
            .unwrap()
            .join("models--org--repo");
        let b1 = repo.join("blobs/b1");
        let b2 = repo.join("blobs/b2");
        let s1 = repo.join("snapshots/rev1/q4.gguf");
        let s2 = repo.join("snapshots/rev1/q8.gguf");
        fs::create_dir_all(b1.parent().unwrap()).unwrap();
        fs::create_dir_all(s1.parent().unwrap()).unwrap();
        fs::write(&b1, vec![1u8; 100]).unwrap();
        fs::write(&b2, vec![2u8; 200]).unwrap();
        symlink(&b1, &s1).unwrap();
        symlink(&b2, &s2).unwrap();

        let mut model = make_file_model(&b1);
        model.source = "HF cache".to_string();

        let plan = plan_delete(&model).unwrap();
        match plan {
            DeletePlan::Files {
                remove,
                prune_dir,
                bytes,
            } => {
                assert_eq!(remove.len(), 2);
                assert!(remove.contains(&s1));
                assert!(remove.contains(&b1));
                assert_eq!(prune_dir, None);
                assert_eq!(bytes, 100);
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn ollama_plan() {
        let model = LocalModel {
            id: "ollama:lfm3:1.7b".to_string(),
            display_name: "lfm3:1.7b".to_string(),
            source: "Ollama".to_string(),
            secondary_label: None,
            path: PathBuf::from("/blobs/sha256-123"),
            size_bytes: 999,
            params: None,
            params_label: "?".to_string(),
            quant: None,
            kind: ModelKind::Chat,
            in_use: false,
            hidden: false,
        };
        assert_eq!(
            plan_delete(&model).unwrap(),
            DeletePlan::Ollama {
                name: "lfm3:1.7b".to_string()
            }
        );
    }

    #[test]
    fn protected_reasons() {
        assert_eq!(
            protected_reason("m1", Some("m1"), Some("m2"), Some("m3")).as_deref(),
            Some("in use by the supervisor")
        );
        assert_eq!(
            protected_reason("m2", Some("m1"), Some("m2"), Some("m3")).as_deref(),
            Some("configured as fallback model")
        );
        assert_eq!(
            protected_reason("m3", Some("m1"), Some("m2"), Some("m3")).as_deref(),
            Some("loaded in the running container")
        );
        assert_eq!(
            protected_reason("m4", Some("m1"), Some("m2"), Some("m3")),
            None
        );
    }

    struct FakeOllama {
        removed: std::sync::Mutex<Vec<String>>,
    }

    impl FakeOllama {
        fn new() -> Self {
            Self {
                removed: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.removed.lock().unwrap().clone()
        }
    }

    impl OllamaRemover for FakeOllama {
        fn remove(&self, name: &str) -> Result<()> {
            self.removed.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }

    #[test]
    fn execute_flat_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tiny.gguf");
        fs::write(&file, vec![1u8; 64]).unwrap();
        let model = make_file_model(&file);
        let plan = plan_delete(&model).unwrap();
        let fake = FakeOllama::new();

        execute_plan(&plan, &fake).unwrap();
        assert!(!file.exists());
        assert!(fake.calls().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn execute_hf_prunes_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path())
            .unwrap()
            .join("models--org--repo");
        let blob = repo.join("blobs/sha256-aaa");
        let snap = repo.join("snapshots/rev1/model.gguf");
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        fs::write(&blob, vec![9u8; 64]).unwrap();
        symlink(&blob, &snap).unwrap();

        let model = make_file_model(&blob);
        let plan = plan_delete(&model).unwrap();
        let fake = FakeOllama::new();

        execute_plan(&plan, &fake).unwrap();
        assert!(!repo.exists());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn execute_ollama_calls_remover() {
        let dir = tempfile::tempdir().unwrap();
        let blob = dir.path().join("sha256-123");
        fs::write(&blob, b"keep-me").unwrap();
        let plan = DeletePlan::Ollama {
            name: "lfm3:1.7b".to_string(),
        };
        let fake = FakeOllama::new();

        execute_plan(&plan, &fake).unwrap();
        assert_eq!(fake.calls(), vec!["lfm3:1.7b".to_string()]);
        assert!(blob.exists());
    }
}
