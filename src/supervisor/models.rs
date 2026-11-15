use super::config::SupervisorConfig;
use super::container::{
    default_ollama_roots, resolve_ollama_gguf_blob, LlmContainerConfig, ModelId,
};
use super::gguf::{file_type_label, read_gguf_info_path, GgufInfo};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const FOLDER_MAX_DEPTH: usize = 4;
const FOLDER_MAX_FILES_PER_ROOT: usize = 2000;

/// Classification of a local GGUF model.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    Chat,
    Embedding,
    Projector,
}

/// A local GGUF model discovered on disk.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct LocalModel {
    pub id: String,
    pub display_name: String,
    pub source: String,
    pub secondary_label: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub params: Option<u64>,
    pub params_label: String,
    pub quant: Option<String>,
    pub kind: ModelKind,
    pub in_use: bool,
    pub hidden: bool,
}

/// Injectable root directories for local model scanning.
#[derive(Clone, Debug, Default)]
pub struct ScanRoots {
    pub ollama: Vec<PathBuf>,
    pub hf_hub: Vec<PathBuf>,
    pub llama_cpp: Vec<PathBuf>,
    pub lm_studio: Vec<PathBuf>,
    pub folders: Vec<PathBuf>,
}

fn expand_tilde(raw: &Path) -> PathBuf {
    let s = raw.to_string_lossy();
    if s == "~" {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    raw.to_path_buf()
}

impl ScanRoots {
    /// Builds `ScanRoots` from standard environment variables, `$HOME`, and `cfg.model_dirs`.
    pub fn from_env_and_config(cfg: &LlmContainerConfig) -> Self {
        let home = env::var("HOME").ok().map(PathBuf::from);

        let mut hf_hub = Vec::new();
        if let Ok(hf_home) = env::var("HF_HOME") {
            let trimmed = hf_home.trim();
            if !trimmed.is_empty() {
                hf_hub.push(expand_tilde(Path::new(trimmed)).join("hub"));
            }
        }
        if let Some(ref h) = home {
            hf_hub.push(h.join(".cache").join("huggingface").join("hub"));
        }

        let mut llama_cpp = Vec::new();
        if let Ok(llama_cache) = env::var("LLAMA_CACHE") {
            let trimmed = llama_cache.trim();
            if !trimmed.is_empty() {
                llama_cpp.push(expand_tilde(Path::new(trimmed)));
            }
        }
        if let Some(ref h) = home {
            llama_cpp.push(h.join(".cache").join("llama.cpp"));
            llama_cpp.push(h.join(".cache").join("tchelvis").join("llama.cpp"));
        }

        let mut lm_studio = Vec::new();
        if let Some(ref h) = home {
            lm_studio.push(h.join(".lmstudio").join("models"));
            lm_studio.push(h.join(".cache").join("lm-studio").join("models"));
        }

        let mut folders = Vec::new();
        if let Some(dirs_env) = crate::envvars::var("MODEL_DIRS") {
            for part in dirs_env.split(':') {
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    folders.push(expand_tilde(Path::new(trimmed)));
                }
            }
        }
        for dir in &cfg.model_dirs {
            if !dir.as_os_str().is_empty() {
                folders.push(expand_tilde(dir));
            }
        }

        Self {
            ollama: default_ollama_roots(),
            hf_hub,
            llama_cpp,
            lm_studio,
            folders,
        }
    }
}

/// Parses parameter labels like `"1.7B"` or `"135M"` into parameter counts.
pub fn parse_params_label(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let upper = trimmed.to_ascii_uppercase();
    let (num_str, mult) = if let Some(n) = upper.strip_suffix('B') {
        (n, 1_000_000_000.0)
    } else if let Some(n) = upper.strip_suffix('M') {
        (n, 1_000_000.0)
    } else if let Some(n) = upper.strip_suffix('K') {
        (n, 1_000.0)
    } else {
        return None;
    };
    let val: f64 = num_str.trim().parse().ok()?;
    if !val.is_finite() || val <= 0.0 {
        return None;
    }
    Some((val * mult).round() as u64)
}

/// Formats a parameter count as `"1.7B"` (>= 1e9) or `"135M"` (< 1e9).
pub fn format_params(n: u64) -> String {
    if n >= 1_000_000_000 {
        let label = format!("{:.1}", (n as f64) / 1_000_000_000.0);
        format!("{}B", label.strip_suffix(".0").unwrap_or(&label))
    } else {
        format!("{}M", ((n as f64) / 1_000_000.0).round() as u64)
    }
}

fn shard_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)-(\d{5})-of-(\d{5})\.gguf$").unwrap())
}

fn is_gguf_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false)
}

fn classify_kind(info: &GgufInfo, filename: &str) -> ModelKind {
    let arch = info
        .architecture
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let gtype = info
        .general_type
        .as_deref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if arch == "clip" || gtype == "mmproj" || filename.to_ascii_lowercase().contains("mmproj") {
        return ModelKind::Projector;
    }
    if matches!(
        arch.as_str(),
        "bert" | "nomic-bert" | "jina-bert-v2" | "t5encoder"
    ) || info.has_pooling
    {
        return ModelKind::Embedding;
    }
    ModelKind::Chat
}

fn source_priority(source: &str) -> u8 {
    match source {
        "Ollama" => 0,
        "LM Studio" => 1,
        "HF cache" => 2,
        "llama.cpp cache" => 3,
        "Folder" => 4,
        _ => 5,
    }
}

fn insert_dedup(map: &mut HashMap<PathBuf, LocalModel>, model: LocalModel) {
    match map.get(&model.path) {
        Some(existing) if source_priority(&existing.source) <= source_priority(&model.source) => {}
        _ => {
            map.insert(model.path.clone(), model);
        }
    }
}

fn build_file_entry(
    raw_path: &Path,
    source: &str,
    secondary_label: Option<String>,
) -> Option<LocalModel> {
    let file_name = raw_path.file_name()?.to_string_lossy().into_owned();
    if !is_gguf_file(raw_path) {
        return None;
    }

    let mut total_size: Option<u64> = None;
    let mut is_shard = false;
    let mut display_name = raw_path.file_stem()?.to_string_lossy().into_owned();

    if let Some(caps) = shard_regex().captures(&file_name) {
        let shard_idx = caps.get(1)?.as_str();
        if shard_idx != "00001" {
            return None;
        }
        is_shard = true;
        let suffix_start = caps.get(0)?.start();
        display_name = file_name[..suffix_start].to_string();

        if let (Some(parent), Ok(total_shards)) =
            (raw_path.parent(), caps.get(2)?.as_str().parse::<u32>())
        {
            let mut sum = 0u64;
            for idx in 1..=total_shards.min(1000) {
                let sibling_name = format!("{display_name}-{idx:05}-of-{total_shards:05}.gguf");
                if let Ok(meta) = fs::metadata(parent.join(sibling_name)) {
                    sum = sum.saturating_add(meta.len());
                }
            }
            total_size = Some(sum);
        }
    }

    let canon_path = fs::canonicalize(raw_path).ok()?;
    let size_bytes =
        total_size.unwrap_or_else(|| fs::metadata(&canon_path).map(|m| m.len()).unwrap_or(0));

    let (params, quant, kind) = match read_gguf_info_path(&canon_path) {
        Ok(info) => {
            let p = info
                .size_label
                .as_deref()
                .and_then(parse_params_label)
                .or_else(|| {
                    (!is_shard && info.parameter_count > 0).then_some(info.parameter_count)
                });
            let q = info
                .file_type
                .and_then(file_type_label)
                .map(|s| s.to_string());
            let k = classify_kind(&info, &file_name);
            (p, q, k)
        }
        Err(_) => (None, None, ModelKind::Chat),
    };

    let params_label = params.map(format_params).unwrap_or_else(|| "?".to_string());
    let id = format!("file:{}", canon_path.display());

    Some(LocalModel {
        id,
        display_name,
        source: source.to_string(),
        secondary_label,
        path: canon_path,
        size_bytes,
        params,
        params_label,
        quant,
        kind,
        in_use: false,
        hidden: false,
    })
}

fn read_dir_entries(dir: &Path) -> Vec<fs::DirEntry> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok()).collect()
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn scan_ollama_root(root: &Path, out: &mut HashMap<PathBuf, LocalModel>) {
    let manifests_dir = root.join("manifests");
    for reg_entry in read_dir_entries(&manifests_dir) {
        let Ok(ft) = reg_entry.file_type() else {
            continue;
        };
        if !ft.is_dir() || is_hidden(&reg_entry.file_name()) {
            continue;
        }
        let registry = reg_entry.file_name().to_string_lossy().into_owned();
        for ns_entry in read_dir_entries(&reg_entry.path()) {
            let Ok(ft) = ns_entry.file_type() else {
                continue;
            };
            if !ft.is_dir() || is_hidden(&ns_entry.file_name()) {
                continue;
            }
            let ns = ns_entry.file_name().to_string_lossy().into_owned();
            for name_entry in read_dir_entries(&ns_entry.path()) {
                let Ok(ft) = name_entry.file_type() else {
                    continue;
                };
                if !ft.is_dir() || is_hidden(&name_entry.file_name()) {
                    continue;
                }
                let name = name_entry.file_name().to_string_lossy().into_owned();
                for tag_entry in read_dir_entries(&name_entry.path()) {
                    let Ok(ft) = tag_entry.file_type() else {
                        continue;
                    };
                    if ft.is_dir() || is_hidden(&tag_entry.file_name()) {
                        continue;
                    }
                    let tag = tag_entry.file_name().to_string_lossy().into_owned();
                    let display_name = if registry == "registry.ollama.ai" && ns == "library" {
                        format!("{name}:{tag}")
                    } else {
                        format!("{registry}/{ns}/{name}:{tag}")
                    };
                    if let Some(model) =
                        parse_ollama_manifest(root, &tag_entry.path(), &display_name)
                    {
                        insert_dedup(out, model);
                    }
                }
            }
        }
    }
}

fn parse_ollama_manifest(
    root: &Path,
    manifest_path: &Path,
    display_name: &str,
) -> Option<LocalModel> {
    let raw = fs::read_to_string(manifest_path).ok()?;
    let val: Value = serde_json::from_str(&raw).ok()?;
    let layers = val.get("layers")?.as_array()?;

    let mut model_digest: Option<&str> = None;
    let mut layer_size: Option<u64> = None;
    let mut config_digest: Option<&str> = val
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|d| d.as_str());

    for layer in layers {
        let mt = layer
            .get("mediaType")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        if mt == "application/vnd.ollama.image.model" {
            model_digest = layer.get("digest").and_then(|d| d.as_str());
            layer_size = layer.get("size").and_then(|s| s.as_u64());
        } else if config_digest.is_none() && mt == "application/vnd.docker.container.image.v1+json"
        {
            config_digest = layer.get("digest").and_then(|d| d.as_str());
        }
    }

    let blob_path = root.join("blobs").join(model_digest?.replace(':', "-"));
    if !blob_path.is_file() {
        return None;
    }
    let canon_blob = fs::canonicalize(&blob_path).ok()?;
    let size_bytes =
        layer_size.unwrap_or_else(|| fs::metadata(&canon_blob).map(|m| m.len()).unwrap_or(0));

    let mut params: Option<u64> = None;
    let mut params_label: Option<String> = None;
    let mut quant: Option<String> = None;
    let mut kind: Option<ModelKind> = None;

    if let Some(cfg_dig) = config_digest {
        let cfg_path = root.join("blobs").join(cfg_dig.replace(':', "-"));
        if let Ok(cfg_raw) = fs::read_to_string(cfg_path) {
            if let Ok(cfg_val) = serde_json::from_str::<Value>(&cfg_raw) {
                if let Some(mt) = cfg_val.get("model_type").and_then(|v| v.as_str()) {
                    if let Some(p) = parse_params_label(mt) {
                        params = Some(p);
                        params_label = Some(mt.trim().to_string());
                    }
                }
                if let Some(ft) = cfg_val.get("file_type").and_then(|v| v.as_str()) {
                    if !ft.trim().is_empty() {
                        quant = Some(ft.trim().to_string());
                    }
                }
                if let Some(fam) = cfg_val.get("model_family").and_then(|v| v.as_str()) {
                    if fam.eq_ignore_ascii_case("bert") || fam.eq_ignore_ascii_case("nomic-bert") {
                        kind = Some(ModelKind::Embedding);
                    }
                }
            }
        }
    }

    if params.is_none() || kind.is_none() || quant.is_none() {
        if let Ok(info) = read_gguf_info_path(&canon_blob) {
            if params.is_none() {
                params = info
                    .size_label
                    .as_deref()
                    .and_then(parse_params_label)
                    .or_else(|| (info.parameter_count > 0).then_some(info.parameter_count));
            }
            if quant.is_none() {
                quant = info
                    .file_type
                    .and_then(file_type_label)
                    .map(|s| s.to_string());
            }
            if kind.is_none() {
                kind = Some(classify_kind(&info, display_name));
            }
        }
    }

    let final_label = params_label
        .or_else(|| params.map(format_params))
        .unwrap_or_else(|| "?".to_string());

    Some(LocalModel {
        id: format!("ollama:{display_name}"),
        display_name: display_name.to_string(),
        source: "Ollama".to_string(),
        secondary_label: None,
        path: canon_blob,
        size_bytes,
        params,
        params_label: final_label,
        quant,
        kind: kind.unwrap_or(ModelKind::Chat),
        in_use: false,
        hidden: false,
    })
}

fn scan_lm_studio_root(root: &Path, out: &mut HashMap<PathBuf, LocalModel>) {
    for pub_entry in read_dir_entries(root) {
        let Ok(ft) = pub_entry.file_type() else {
            continue;
        };
        if !ft.is_dir() || is_hidden(&pub_entry.file_name()) {
            continue;
        }
        let publisher = pub_entry.file_name().to_string_lossy().into_owned();
        for repo_entry in read_dir_entries(&pub_entry.path()) {
            let Ok(ft) = repo_entry.file_type() else {
                continue;
            };
            if !ft.is_dir() || is_hidden(&repo_entry.file_name()) {
                continue;
            }
            let repo = repo_entry.file_name().to_string_lossy().into_owned();
            let secondary = format!("{publisher}/{repo}");
            for file_entry in read_dir_entries(&repo_entry.path()) {
                let Ok(ft) = file_entry.file_type() else {
                    continue;
                };
                if ft.is_dir() || is_hidden(&file_entry.file_name()) {
                    continue;
                }
                if let Some(model) =
                    build_file_entry(&file_entry.path(), "LM Studio", Some(secondary.clone()))
                {
                    insert_dedup(out, model);
                }
            }
        }
    }
}

fn scan_hf_hub_layout(root: &Path, source: &str, out: &mut HashMap<PathBuf, LocalModel>) {
    for entry in read_dir_entries(root) {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = dir_name.strip_prefix("models--") else {
            continue;
        };
        let Some((org, repo)) = rest.split_once("--") else {
            continue;
        };
        let secondary = format!("{org}/{repo}");
        let snapshots_dir = entry.path().join("snapshots");
        for rev_entry in read_dir_entries(&snapshots_dir) {
            let Ok(ft) = rev_entry.file_type() else {
                continue;
            };
            if !ft.is_dir() || is_hidden(&rev_entry.file_name()) {
                continue;
            }
            let mut seen_files = 0usize;
            walk_folder_recursive(
                &rev_entry.path(),
                1,
                FOLDER_MAX_DEPTH,
                &mut seen_files,
                &mut |file_path| {
                    if let Some(model) =
                        build_file_entry(file_path, source, Some(secondary.clone()))
                    {
                        insert_dedup(out, model);
                    }
                },
            );
        }
    }
}

fn scan_llama_cpp_root(root: &Path, out: &mut HashMap<PathBuf, LocalModel>) {
    // Flat *.gguf files
    for entry in read_dir_entries(root) {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() || is_hidden(&entry.file_name()) {
            continue;
        }
        if let Some(model) = build_file_entry(&entry.path(), "llama.cpp cache", None) {
            insert_dedup(out, model);
        }
    }
    // HF-hub layout inside llama.cpp cache
    scan_hf_hub_layout(root, "llama.cpp cache", out);
}

fn walk_folder_recursive(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    seen_files: &mut usize,
    on_file: &mut impl FnMut(&Path),
) {
    if depth > max_depth || *seen_files >= FOLDER_MAX_FILES_PER_ROOT {
        return;
    }
    for entry in read_dir_entries(dir) {
        if *seen_files >= FOLDER_MAX_FILES_PER_ROOT {
            break;
        }
        let name = entry.file_name();
        if is_hidden(&name) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            // Follow symlinked files (e.g. HF snapshots), never symlinked directories.
            let path = entry.path();
            if let Ok(meta) = fs::metadata(&path) {
                if meta.is_file() {
                    *seen_files += 1;
                    on_file(&path);
                }
            }
            continue;
        }
        if ft.is_dir() {
            walk_folder_recursive(&entry.path(), depth + 1, max_depth, seen_files, on_file);
        } else if ft.is_file() {
            *seen_files += 1;
            on_file(&entry.path());
        }
    }
}

fn scan_folder_root(root: &Path, out: &mut HashMap<PathBuf, LocalModel>) {
    let mut seen_files = 0usize;
    walk_folder_recursive(
        root,
        1,
        FOLDER_MAX_DEPTH,
        &mut seen_files,
        &mut |file_path| {
            if let Some(model) = build_file_entry(file_path, "Folder", None) {
                insert_dedup(out, model);
            }
        },
    );
}

/// Scans all configured roots for local GGUF models, deduplicated by canonical path.
pub fn scan_local_models(roots: &ScanRoots) -> Vec<LocalModel> {
    let mut by_path: HashMap<PathBuf, LocalModel> = HashMap::new();

    for r in &roots.ollama {
        scan_ollama_root(r, &mut by_path);
    }
    for r in &roots.lm_studio {
        scan_lm_studio_root(r, &mut by_path);
    }
    for r in &roots.hf_hub {
        scan_hf_hub_layout(r, "HF cache", &mut by_path);
    }
    for r in &roots.llama_cpp {
        scan_llama_cpp_root(r, &mut by_path);
    }
    for r in &roots.folders {
        scan_folder_root(r, &mut by_path);
    }

    let mut list: Vec<LocalModel> = by_path.into_values().collect();
    list.sort_by(|a, b| {
        source_priority(&a.source)
            .cmp(&source_priority(&b.source))
            .then_with(|| {
                a.display_name
                    .to_ascii_lowercase()
                    .cmp(&b.display_name.to_ascii_lowercase())
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    list
}

/// Parameter-count filter bucket for `model list`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MaxParams {
    B2,
    B4,
    B8,
    All,
}

impl MaxParams {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "2b" => Some(Self::B2),
            "4b" => Some(Self::B4),
            "8b" => Some(Self::B8),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::B2 => "2b",
            Self::B4 => "4b",
            Self::B8 => "8b",
            Self::All => "all",
        }
    }

    pub fn limit(&self) -> Option<u64> {
        match self {
            Self::B2 => Some(2_500_000_000),
            Self::B4 => Some(4_500_000_000),
            Self::B8 => Some(8_500_000_000),
            Self::All => None,
        }
    }
}

/// The currently effective model produced by `resolve_model_source` precedence.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveModel {
    pub id: String,
    pub display_name: String,
    pub path: Option<PathBuf>,
}

/// JSON output of `tchelvis supervisor model list --json` and `filter`.
#[derive(Serialize, Debug)]
pub struct ModelListOutput {
    pub models: Vec<LocalModel>,
    pub hidden_count: usize,
    pub user_hidden_count: usize,
    pub filter: String,
    pub selected_id: Option<String>,
    pub managed: bool,
    pub env_override_ignored: Option<String>,
}

/// Marks `in_use` and `hidden`, injects a synthetic `Config` row if `effective` is absent, and applies filtering.
pub fn build_model_list(
    mut scanned: Vec<LocalModel>,
    filter: MaxParams,
    effective: Option<&EffectiveModel>,
    show_all: bool,
    hidden: &[String],
) -> (Vec<LocalModel>, usize, usize) {
    let mut matched = false;
    if let Some(eff) = effective {
        for m in &mut scanned {
            if m.id == eff.id {
                m.in_use = true;
                matched = true;
            }
        }
        if !matched {
            let size_bytes = eff
                .path
                .as_ref()
                .and_then(|p| fs::metadata(p).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            scanned.push(LocalModel {
                id: eff.id.clone(),
                display_name: eff.display_name.clone(),
                source: "Config".to_string(),
                secondary_label: None,
                path: eff.path.clone().unwrap_or_default(),
                size_bytes,
                params: None,
                params_label: "?".to_string(),
                quant: None,
                kind: ModelKind::Chat,
                in_use: true,
                hidden: false,
            });
        }
    }

    let hidden_set: std::collections::HashSet<&str> = hidden.iter().map(String::as_str).collect();
    let mut user_hidden_count = 0usize;
    for m in &mut scanned {
        if !m.in_use && hidden_set.contains(m.id.as_str()) {
            m.hidden = true;
            user_hidden_count += 1;
        } else {
            m.hidden = false;
        }
    }

    if show_all {
        return (scanned, 0, user_hidden_count);
    }

    let limit = filter.limit();
    let mut kept = Vec::with_capacity(scanned.len());
    let mut filter_hidden_count = 0usize;

    for m in scanned {
        if m.in_use {
            kept.push(m);
            continue;
        }
        if m.hidden {
            continue;
        }
        if m.kind != ModelKind::Chat {
            filter_hidden_count += 1;
            continue;
        }
        if let (Some(max), Some(p)) = (limit, m.params) {
            if p > max {
                filter_hidden_count += 1;
                continue;
            }
        }
        kept.push(m);
    }

    (kept, filter_hidden_count, user_hidden_count)
}

fn file_display_name(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| p.display().to_string())
}

/// Computes the effective model following container resolution precedence:
/// `model_id` > `model_path` > `hf_repo` > `ollama_model`.
pub fn effective_model(cfg: &SupervisorConfig, ollama_roots: &[PathBuf]) -> Option<EffectiveModel> {
    if let Some(ref raw_id) = cfg.llm_container.model_id {
        let trimmed = raw_id.trim();
        if !trimmed.is_empty() {
            if let Ok(parsed) = ModelId::parse(trimmed) {
                return match parsed {
                    ModelId::Ollama(name) => Some(EffectiveModel {
                        id: format!("ollama:{name}"),
                        display_name: name.clone(),
                        path: resolve_ollama_gguf_blob(&name, ollama_roots),
                    }),
                    ModelId::File(path) => Some(EffectiveModel {
                        id: format!("file:{}", path.display()),
                        display_name: file_display_name(&path),
                        path: Some(path),
                    }),
                    ModelId::Hf(repo) => Some(EffectiveModel {
                        id: format!("hf:{repo}"),
                        display_name: repo,
                        path: None,
                    }),
                };
            }
        }
    }

    if let Some(ref path) = cfg.llm_container.model_path {
        if !path.as_os_str().is_empty() {
            return Some(EffectiveModel {
                id: format!("file:{}", path.display()),
                display_name: file_display_name(path),
                path: Some(path.clone()),
            });
        }
    }

    if let Some(ref repo) = cfg.llm_container.hf_repo {
        let trimmed = repo.trim();
        if !trimmed.is_empty() {
            return Some(EffectiveModel {
                id: format!("hf:{trimmed}"),
                display_name: trimmed.to_string(),
                path: None,
            });
        }
    }

    let ollama_name = cfg.ollama_model.trim();
    if !ollama_name.is_empty() {
        return Some(EffectiveModel {
            id: format!("ollama:{ollama_name}"),
            display_name: ollama_name.to_string(),
            path: resolve_ollama_gguf_blob(ollama_name, ollama_roots),
        });
    }

    None
}

/// Returns `"<VAR>=<value>"` when `model_id_set` is true and an env var overrides
/// a different model name than `effective_display`. Checks `TCHELVIS_LLM_MODEL` before `OLLAMA_MODEL`.
pub fn env_override_note(
    model_id_set: bool,
    effective_display: &str,
    tchelvis_llm_model: Option<&str>,
    ollama_model_env: Option<&str>,
) -> Option<String> {
    if !model_id_set {
        return None;
    }
    let (var_name, val) =
        if let Some(v) = tchelvis_llm_model.map(str::trim).filter(|s| !s.is_empty()) {
            ("TCHELVIS_LLM_MODEL", v)
        } else if let Some(v) = ollama_model_env.map(str::trim).filter(|s| !s.is_empty()) {
            ("OLLAMA_MODEL", v)
        } else {
            return None;
        };

    if val != effective_display {
        Some(format!("{var_name}={val}"))
    } else {
        None
    }
}

/// Validates that a requested model `id` is either in `scanned_ids` or equals `effective_id`.
pub fn validate_model_choice(
    id: &str,
    scanned_ids: &[String],
    effective_id: Option<&str>,
) -> anyhow::Result<()> {
    if scanned_ids.iter().any(|s| s == id) || effective_id == Some(id) {
        Ok(())
    } else {
        anyhow::bail!("Modelo não encontrado: {id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::gguf::tests::GgufBuilder;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn write_gguf(path: &Path, bytes: &[u8]) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    fn empty_roots() -> ScanRoots {
        ScanRoots {
            ollama: vec![],
            hf_hub: vec![],
            llama_cpp: vec![],
            lm_studio: vec![],
            folders: vec![],
        }
    }

    #[test]
    fn scans_ollama_library_and_other_registry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let gguf1 = GgufBuilder::new()
            .kv_str("general.architecture", "llama")
            .tensor("w", &[10, 10])
            .bytes();
        let blob1_digest = "sha256:1111";
        write_gguf(&root.join("blobs/sha256-1111"), &gguf1);
        let cfg1 = r#"{"model_type":"1.7B","file_type":"Q4_K_M","model_family":"llama"}"#;
        fs::write(root.join("blobs/sha256-cfg1"), cfg1).unwrap();
        let man1 = format!(
            r#"{{"config":{{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"sha256:cfg1"}},"layers":[{{"mediaType":"application/vnd.ollama.image.model","digest":"{blob1_digest}","size":123456}}]}}"#
        );
        let man1_path = root.join("manifests/registry.ollama.ai/library/lfm3/1.7b");
        fs::create_dir_all(man1_path.parent().unwrap()).unwrap();
        fs::write(man1_path, man1).unwrap();

        let gguf2 = GgufBuilder::new()
            .kv_str("general.architecture", "qwen2")
            .kv_str("general.size_label", "3.0B")
            .kv_u32("general.file_type", 7)
            .bytes();
        write_gguf(&root.join("blobs/sha256-2222"), &gguf2);
        let man2 = r#"{"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:2222","size":999}]}"#;
        let man2_path = root.join("manifests/hf.co/org/repo/Q8_0");
        fs::create_dir_all(man2_path.parent().unwrap()).unwrap();
        fs::write(man2_path, man2).unwrap();

        let mut roots = empty_roots();
        roots.ollama.push(root.to_path_buf());

        let models = scan_local_models(&roots);
        assert_eq!(models.len(), 2);

        let m1 = models
            .iter()
            .find(|m| m.display_name == "lfm3:1.7b")
            .unwrap();
        assert_eq!(m1.id, "ollama:lfm3:1.7b");
        assert_eq!(m1.source, "Ollama");
        assert_eq!(m1.params, Some(1_700_000_000));
        assert_eq!(m1.params_label, "1.7B");
        assert_eq!(m1.quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(m1.size_bytes, 123456);
        assert_eq!(m1.kind, ModelKind::Chat);

        let m2 = models
            .iter()
            .find(|m| m.display_name == "hf.co/org/repo:Q8_0")
            .unwrap();
        assert_eq!(m2.id, "ollama:hf.co/org/repo:Q8_0");
        assert_eq!(m2.source, "Ollama");
        assert_eq!(m2.params, Some(3_000_000_000));
        assert_eq!(m2.quant.as_deref(), Some("Q8_0"));
    }

    #[test]
    fn ollama_bert_family_is_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let gguf = GgufBuilder::new()
            .kv_str("general.architecture", "llama")
            .bytes();
        write_gguf(&root.join("blobs/sha256-aaaa"), &gguf);
        let cfg = r#"{"model_type":"137M","model_family":"nomic-bert"}"#;
        fs::write(root.join("blobs/sha256-cfga"), cfg).unwrap();
        let man = r#"{"config":{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"sha256:cfga"},"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:aaaa","size":500}]}"#;
        let man_path = root.join("manifests/registry.ollama.ai/library/nomic-embed-text/latest");
        fs::create_dir_all(man_path.parent().unwrap()).unwrap();
        fs::write(man_path, man).unwrap();

        let mut roots = empty_roots();
        roots.ollama.push(root.to_path_buf());

        let models = scan_local_models(&roots);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].kind, ModelKind::Embedding);
    }

    #[cfg(unix)]
    #[test]
    fn scans_hf_hub_snapshot_symlink_and_dedupes_with_llama_cpp_cache() {
        let dir = tempfile::tempdir().unwrap();
        let hf_root = dir.path().join("hf_hub");
        let llama_root = dir.path().join("llama_cpp");
        fs::create_dir_all(&llama_root).unwrap();

        let blob = hf_root.join("models--org--repo/blobs/deadbeef");
        let gguf = GgufBuilder::new()
            .kv_str("general.size_label", "2.0B")
            .kv_u32("general.file_type", 15)
            .bytes();
        write_gguf(&blob, &gguf);

        let snap = hf_root.join("models--org--repo/snapshots/rev1/model.gguf");
        fs::create_dir_all(snap.parent().unwrap()).unwrap();
        symlink(&blob, &snap).unwrap();

        let llama_link = llama_root.join("model.gguf");
        symlink(&blob, &llama_link).unwrap();

        let mut roots = empty_roots();
        roots.hf_hub.push(hf_root);
        roots.llama_cpp.push(llama_root);

        let models = scan_local_models(&roots);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].source, "HF cache");
        assert_eq!(models[0].secondary_label.as_deref(), Some("org/repo"));
        assert_eq!(models[0].display_name, "model");
    }

    #[test]
    fn scans_lm_studio_tree() {
        let dir = tempfile::tempdir().unwrap();
        let lms_root = dir.path().join("lmstudio");
        let gguf = GgufBuilder::new()
            .kv_str("general.size_label", "4.0B")
            .bytes();
        write_gguf(
            &lms_root.join("bartowski/Qwen2.5-3B-GGUF/Qwen2.5-3B-Q4_K_M.gguf"),
            &gguf,
        );

        let mut roots = empty_roots();
        roots.lm_studio.push(lms_root);

        let models = scan_local_models(&roots);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].source, "LM Studio");
        assert_eq!(
            models[0].secondary_label.as_deref(),
            Some("bartowski/Qwen2.5-3B-GGUF")
        );
        assert_eq!(models[0].display_name, "Qwen2.5-3B-Q4_K_M");
    }

    #[cfg(unix)]
    #[test]
    fn folder_scan_respects_depth_hidden_and_symlink_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        let gguf = GgufBuilder::new()
            .kv_str("general.size_label", "1.0B")
            .bytes();

        // Depth 1 (included)
        write_gguf(&root.join("ok1.gguf"), &gguf);
        // Depth 4 (included): root/a/b/c/ok4.gguf
        write_gguf(&root.join("a/b/c/ok4.gguf"), &gguf);
        // Depth 5 (excluded): root/a/b/c/d/too_deep.gguf
        write_gguf(&root.join("a/b/c/d/too_deep.gguf"), &gguf);
        // Hidden dir (excluded)
        write_gguf(&root.join(".hidden/secret.gguf"), &gguf);
        // Symlinked dir loop (excluded, must terminate)
        let loop_dir = root.join("loop");
        symlink(&root, &loop_dir).unwrap();

        let mut roots = empty_roots();
        roots.folders.push(root);

        let models = scan_local_models(&roots);
        let names: Vec<&str> = models.iter().map(|m| m.display_name.as_str()).collect();
        assert_eq!(names, vec!["ok1", "ok4"]);
    }

    #[test]
    fn split_shards_listed_once_with_summed_size() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        let gguf1 = GgufBuilder::new()
            .kv_str("general.size_label", "7.0B")
            .tensor("t", &[100, 100])
            .bytes();
        let shard1 = root.join("big-model-00001-of-00003.gguf");
        let shard2 = root.join("big-model-00002-of-00003.gguf");
        let shard3 = root.join("big-model-00003-of-00003.gguf");
        write_gguf(&shard1, &gguf1);
        write_gguf(&shard2, &[0u8; 200]);
        write_gguf(&shard3, &[0u8; 300]);

        let mut roots = empty_roots();
        roots.folders.push(root);

        let models = scan_local_models(&roots);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "big-model");
        assert_eq!(models[0].size_bytes, gguf1.len() as u64 + 200 + 300);
        assert_eq!(models[0].params, Some(7_000_000_000));
    }

    #[test]
    fn kind_detection_projector_and_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");

        let proj_bytes = GgufBuilder::new()
            .kv_str("general.architecture", "llama")
            .bytes();
        write_gguf(&root.join("mmproj-x.gguf"), &proj_bytes);

        let emb_bytes = GgufBuilder::new()
            .kv_str("general.architecture", "llama")
            .kv_u32("llama.pooling_type", 1)
            .bytes();
        write_gguf(&root.join("embed.gguf"), &emb_bytes);

        let mut roots = empty_roots();
        roots.folders.push(root);

        let models = scan_local_models(&roots);
        let proj = models
            .iter()
            .find(|m| m.display_name == "mmproj-x")
            .unwrap();
        assert_eq!(proj.kind, ModelKind::Projector);
        let emb = models.iter().find(|m| m.display_name == "embed").unwrap();
        assert_eq!(emb.kind, ModelKind::Embedding);
    }

    #[test]
    fn scan_lists_corrupt_gguf_with_unknown_params() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        write_gguf(&root.join("junk.gguf"), b"not a gguf file at all");

        let mut roots = empty_roots();
        roots.folders.push(root);

        let models = scan_local_models(&roots);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "junk");
        assert_eq!(models[0].params, None);
        assert_eq!(models[0].params_label, "?");
        assert_eq!(models[0].kind, ModelKind::Chat);
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_unreadable_root() {
        // Skip if running as root since chmod 0o000 does not block root.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        let ok = dir.path().join("ok");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let gguf = GgufBuilder::new()
            .kv_str("general.size_label", "1.0B")
            .bytes();
        write_gguf(&ok.join("good.gguf"), &gguf);

        let mut roots = empty_roots();
        roots.folders.push(locked.clone());
        roots.folders.push(ok);

        let models = scan_local_models(&roots);
        // Restore permissions so tempdir cleanup succeeds.
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "good");
    }

    #[test]
    fn params_label_roundtrip() {
        assert_eq!(parse_params_label("1.7B"), Some(1_700_000_000));
        assert_eq!(parse_params_label("135M"), Some(135_000_000));
        assert_eq!(format_params(3_090_000_000), "3.1B");
        assert_eq!(format_params(135_000_000), "135M");
        assert_eq!(format_params(7_000_000_000), "7B");
    }

    fn dummy_model(id: &str, params: Option<u64>, kind: ModelKind) -> LocalModel {
        LocalModel {
            id: id.to_string(),
            display_name: id.to_string(),
            source: "Folder".to_string(),
            secondary_label: None,
            path: PathBuf::from(format!("/m/{id}.gguf")),
            size_bytes: 100,
            params,
            params_label: params.map(format_params).unwrap_or_else(|| "?".to_string()),
            quant: None,
            kind,
            in_use: false,
            hidden: false,
        }
    }

    #[test]
    fn filter_thresholds() {
        let base = vec![
            dummy_model("m24", Some(2_400_000_000), ModelKind::Chat),
            dummy_model("m26", Some(2_600_000_000), ModelKind::Chat),
            dummy_model("m44", Some(4_400_000_000), ModelKind::Chat),
            dummy_model("m86", Some(8_600_000_000), ModelKind::Chat),
        ];

        let (b2, h2, _) = build_model_list(base.clone(), MaxParams::B2, None, false, &[]);
        assert_eq!(b2.len(), 1);
        assert_eq!(b2[0].id, "m24");
        assert_eq!(h2, 3);

        let (b4, h4, _) = build_model_list(base.clone(), MaxParams::B4, None, false, &[]);
        assert_eq!(b4.len(), 3);
        assert_eq!(h4, 1);

        let (b8, h8, _) = build_model_list(base.clone(), MaxParams::B8, None, false, &[]);
        assert_eq!(b8.len(), 3);
        assert_eq!(h8, 1);

        let (all, ha, _) = build_model_list(base, MaxParams::All, None, false, &[]);
        assert_eq!(all.len(), 4);
        assert_eq!(ha, 0);
    }

    #[test]
    fn in_use_always_shown() {
        let base = vec![dummy_model("m7b", Some(7_000_000_000), ModelKind::Chat)];
        let eff = EffectiveModel {
            id: "m7b".to_string(),
            display_name: "m7b".to_string(),
            path: None,
        };
        let (list, hidden, _) = build_model_list(base, MaxParams::B4, Some(&eff), false, &[]);
        assert_eq!(list.len(), 1);
        assert!(list[0].in_use);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn unknown_params_shown() {
        let base = vec![dummy_model("unk", None, ModelKind::Chat)];
        let (list, hidden, _) = build_model_list(base, MaxParams::B2, None, false, &[]);
        assert_eq!(list.len(), 1);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn embeddings_hidden_and_counted() {
        let base = vec![
            dummy_model("chat", Some(1_000_000_000), ModelKind::Chat),
            dummy_model("emb", Some(135_000_000), ModelKind::Embedding),
            dummy_model("proj", Some(500_000_000), ModelKind::Projector),
        ];
        let (filtered, hidden, _) = build_model_list(base.clone(), MaxParams::B4, None, false, &[]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(hidden, 2);

        let (all, hidden_all, _) = build_model_list(base, MaxParams::B4, None, true, &[]);
        assert_eq!(all.len(), 3);
        assert_eq!(hidden_all, 0);
    }

    #[test]
    fn missing_effective_gets_config_row() {
        let base = vec![dummy_model("other", Some(1_000_000_000), ModelKind::Chat)];
        let eff = EffectiveModel {
            id: "ollama:gone:1b".to_string(),
            display_name: "gone:1b".to_string(),
            path: None,
        };
        let (list, hidden, _) = build_model_list(base, MaxParams::B4, Some(&eff), false, &[]);
        assert_eq!(hidden, 0);
        let last = list.last().unwrap();
        assert_eq!(last.source, "Config");
        assert_eq!(last.id, "ollama:gone:1b");
        assert!(last.in_use);
    }

    #[test]
    fn build_list_drops_user_hidden() {
        let base = vec![
            dummy_model("m1", Some(1_000_000_000), ModelKind::Chat),
            dummy_model("m2", Some(1_500_000_000), ModelKind::Chat),
        ];
        let hidden_ids = vec!["m1".to_string()];
        let (list, filter_hidden, user_hidden) =
            build_model_list(base, MaxParams::B4, None, false, &hidden_ids);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "m2");
        assert!(!list[0].hidden);
        assert_eq!(filter_hidden, 0);
        assert_eq!(user_hidden, 1);
    }

    #[test]
    fn build_list_show_all_marks_hidden() {
        let base = vec![
            dummy_model("m1", Some(1_000_000_000), ModelKind::Chat),
            dummy_model("m2", Some(1_500_000_000), ModelKind::Chat),
        ];
        let hidden_ids = vec!["m1".to_string()];
        let (list, filter_hidden, user_hidden) =
            build_model_list(base, MaxParams::B4, None, true, &hidden_ids);
        assert_eq!(list.len(), 2);
        let m1 = list.iter().find(|m| m.id == "m1").unwrap();
        let m2 = list.iter().find(|m| m.id == "m2").unwrap();
        assert!(m1.hidden);
        assert!(!m2.hidden);
        assert_eq!(filter_hidden, 0);
        assert_eq!(user_hidden, 1);
    }

    #[test]
    fn build_list_never_hides_in_use() {
        let base = vec![dummy_model("m1", Some(1_000_000_000), ModelKind::Chat)];
        let eff = EffectiveModel {
            id: "m1".to_string(),
            display_name: "m1".to_string(),
            path: None,
        };
        let hidden_ids = vec!["m1".to_string()];
        let (list, filter_hidden, user_hidden) =
            build_model_list(base, MaxParams::B4, Some(&eff), false, &hidden_ids);
        assert_eq!(list.len(), 1);
        assert!(list[0].in_use);
        assert!(!list[0].hidden);
        assert_eq!(filter_hidden, 0);
        assert_eq!(user_hidden, 0);
    }

    #[test]
    fn max_params_parse() {
        assert_eq!(MaxParams::parse("4b"), Some(MaxParams::B4));
        assert_eq!(MaxParams::parse("4B"), Some(MaxParams::B4));
        assert_eq!(MaxParams::parse("all"), Some(MaxParams::All));
        assert_eq!(MaxParams::parse("5b"), None);
        assert_eq!(MaxParams::B4.limit(), Some(4_500_000_000));
    }

    #[test]
    fn effective_model_precedence() {
        let mut cfg = crate::supervisor::config::SupervisorConfig {
            ollama_model: "lfm3:1.7b".to_string(),
            ..Default::default()
        };

        // 4. ollama_model fallback
        let eff4 = effective_model(&cfg, &[]).unwrap();
        assert_eq!(eff4.id, "ollama:lfm3:1.7b");
        assert_eq!(eff4.display_name, "lfm3:1.7b");

        // 3. hf_repo
        cfg.llm_container.hf_repo = Some("org/repo:Q4_K_M".to_string());
        let eff3 = effective_model(&cfg, &[]).unwrap();
        assert_eq!(eff3.id, "hf:org/repo:Q4_K_M");
        assert_eq!(eff3.display_name, "org/repo:Q4_K_M");

        // 2. model_path
        cfg.llm_container.model_path = Some(PathBuf::from("/m/local.gguf"));
        let eff2 = effective_model(&cfg, &[]).unwrap();
        assert_eq!(eff2.id, "file:/m/local.gguf");
        assert_eq!(eff2.display_name, "local");

        // 1. model_id
        cfg.llm_container.model_id = Some("ollama:qwen2.5:3b".to_string());
        let eff1 = effective_model(&cfg, &[]).unwrap();
        assert_eq!(eff1.id, "ollama:qwen2.5:3b");
        assert_eq!(eff1.display_name, "qwen2.5:3b");
    }

    #[test]
    fn env_override_note_cases() {
        assert_eq!(
            env_override_note(false, "qwen2.5:3b", None, Some("lfm3:1.7b")),
            None
        );
        assert_eq!(
            env_override_note(true, "lfm3:1.7b", None, Some("lfm3:1.7b")),
            None
        );
        assert_eq!(
            env_override_note(true, "qwen2.5:3b", None, Some("lfm3:1.7b")),
            Some("OLLAMA_MODEL=lfm3:1.7b".to_string())
        );
        assert_eq!(
            env_override_note(true, "qwen2.5:3b", Some("other:1b"), Some("lfm3:1.7b")),
            Some("TCHELVIS_LLM_MODEL=other:1b".to_string())
        );
    }

    #[test]
    fn set_model_rejects_unknown_id() {
        let scanned = vec!["ollama:lfm3:1.7b".to_string(), "file:/m/a.gguf".to_string()];
        assert!(validate_model_choice("ollama:lfm3:1.7b", &scanned, None).is_ok());
        assert!(validate_model_choice("hf:org/repo", &scanned, Some("hf:org/repo")).is_ok());
        let err = validate_model_choice("ollama:unknown:1b", &scanned, None).unwrap_err();
        assert!(err.to_string().contains("ollama:unknown:1b"));
    }
}
