use crate::loops::config::parse_loops_config;
use crate::loops::workflow::{
    line_at, toml_error_reason, validate_model_name, Provider, RoleDefault, RoleDefaults, Stage,
    UnresolvedWorkflow, Workflow,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::str::FromStr;

const MAX_MODELS_TOML_BYTES: usize = 256 * 1024;
const MAX_CONFIRMED_JSON_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ModelChoice {
    pub fn display(&self) -> String {
        match &self.model {
            Some(m) => format!("{}:{m}", self.provider),
            None => self.provider.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    RunFlag,
    ProjectStage,
    Workflow,
    ProjectDefault,
    GlobalDefault,
}

impl ModelSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RunFlag => "--use",
            Self::ProjectStage => "projeto (.loops/models.toml etapa)",
            Self::Workflow => "workflow",
            Self::ProjectDefault => "projeto (.loops/models.toml padrão)",
            Self::GlobalDefault => "global (~/.config/tchelvis/loops.toml)",
        }
    }

    pub fn is_from_repo(self, workflow_origin: WorkflowOrigin) -> bool {
        match self {
            Self::RunFlag | Self::GlobalDefault => false,
            Self::ProjectStage | Self::ProjectDefault => true,
            Self::Workflow => workflow_origin == WorkflowOrigin::Repo,
        }
    }
}

impl fmt::Display for ModelSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowOrigin {
    Repo,
    Global,
    Preset,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProjectModels {
    pub defaults: RoleDefaults,
    pub stages: BTreeMap<String, BTreeMap<String, ModelChoice>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectModels {
    #[serde(default)]
    defaults: RoleDefaults,
    #[serde(default)]
    stages: BTreeMap<String, BTreeMap<String, ModelChoice>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageResolution {
    pub stage_id: String,
    pub role: String,
    pub choice: ModelChoice,
    pub source: ModelSource,
    pub from_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowResolution {
    pub workflow: Workflow,
    pub stages: Vec<StageResolution>,
}

impl WorkflowResolution {
    pub fn model_sources(&self) -> BTreeMap<String, ModelSource> {
        self.stages
            .iter()
            .map(|s| (s.stage_id.clone(), s.source))
            .collect()
    }

    pub fn needs_confirmation(&self) -> bool {
        self.stages.iter().any(|s| s.from_repo)
    }
}

fn validate_key_segment(name: &str, label: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
    {
        bail!("nome inválido em {label}: '{name}'");
    }
    Ok(())
}

pub fn parse_project_models(raw: &str) -> Result<ProjectModels> {
    let parsed: RawProjectModels = toml::from_str(raw).map_err(|e| {
        anyhow::anyhow!(
            "configuração .loops/models.toml inválida ({})",
            toml_error_reason(raw, &e)
        )
    })?;

    for (role, def) in &parsed.defaults {
        validate_key_segment(role, &format!("[defaults.{role}]"))?;
        if let Some(model) = &def.model {
            validate_model_name(model).map_err(|e| anyhow::anyhow!("[defaults.{role}]: {e}"))?;
        }
    }

    for (wf, stage_map) in &parsed.stages {
        validate_key_segment(wf, &format!("[stages.{wf}]"))?;
        for (stage, choice) in stage_map {
            validate_key_segment(stage, &format!("[stages.{wf}.{stage}]"))?;
            if let Some(model) = &choice.model {
                validate_model_name(model)
                    .map_err(|e| anyhow::anyhow!("[stages.{wf}.{stage}]: {e}"))?;
            }
        }
    }

    Ok(ProjectModels {
        defaults: parsed.defaults,
        stages: parsed.stages,
    })
}

pub fn parse_use_flag(raw: &str) -> Result<(String, ModelChoice)> {
    let Some((stage, rhs)) = raw.split_once('=') else {
        bail!("formato inválido em --use '{raw}': esperado etapa=cli[:modelo]");
    };
    if stage.is_empty()
        || stage.len() > 32
        || stage == "done"
        || !stage
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("nome de etapa inválido em --use: '{stage}'");
    }
    let (provider_str, model) = match rhs.split_once(':') {
        Some((p, m)) => {
            validate_model_name(m).map_err(|e| anyhow::anyhow!("--use, etapa '{stage}': {e}"))?;
            (p, Some(m.to_string()))
        }
        None => (rhs, None),
    };
    if provider_str.is_empty() {
        bail!("provider ausente em --use para etapa '{stage}'");
    }
    let provider = Provider::from_str(provider_str).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((stage.to_string(), ModelChoice { provider, model }))
}

fn open_dir_nofollow(
    parent: BorrowedFd<'_>,
    name: &str,
) -> std::result::Result<OwnedFd, std::io::Error> {
    let c_name = CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL byte in name"))?;
    // SAFETY: parent is an open directory fd and c_name is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: fd is a newly opened directory descriptor owned by us.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn read_regular_file_at(
    dir: BorrowedFd<'_>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    let c_name = CString::new(name).context("nome de arquivo contém byte nulo")?;
    // SAFETY: dir is an open directory fd; O_NOFOLLOW blocks symlinks; O_NONBLOCK avoids FIFO blocking.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        if err.raw_os_error() == Some(libc::ELOOP) {
            bail!("arquivo '{name}' é um link simbólico e foi recusado");
        }
        bail!("falha ao abrir '{name}': {err}");
    }
    // SAFETY: fd is a valid newly opened descriptor owned by us.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: owned_fd is valid.
    if unsafe { libc::fstat(owned_fd.as_raw_fd(), &mut st) } < 0 {
        let err = std::io::Error::last_os_error();
        bail!("fstat falhou em '{name}': {err}");
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        bail!("arquivo '{name}' deve ser um arquivo regular");
    }
    if st.st_size < 0 || st.st_size as usize > max_bytes {
        bail!("arquivo '{name}' excede o limite de {max_bytes} bytes");
    }
    let file = File::from(owned_fd);
    let mut buf = Vec::with_capacity(st.st_size as usize);
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut buf)
        .with_context(|| format!("falha ao ler '{name}'"))?;
    if buf.len() > max_bytes {
        bail!("arquivo '{name}' excede o limite de {max_bytes} bytes");
    }
    let text =
        String::from_utf8(buf).with_context(|| format!("arquivo '{name}' não é UTF-8 válido"))?;
    Ok(Some(text))
}

fn open_root_dir(path: &Path) -> Result<OwnedFd> {
    let raw = path
        .to_str()
        .with_context(|| format!("caminho não é UTF-8 válido: {}", path.display()))?;
    let c_path = CString::new(raw).context("caminho contém byte nulo")?;
    // SAFETY: c_path is NUL-terminated; opens existing directory fd.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        bail!("falha ao abrir diretório {}: {err}", path.display());
    }
    // SAFETY: fd is newly opened and owned by us.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn load_project_models_from_repo(repo_root: &Path) -> Result<Option<ProjectModels>> {
    let repo_fd = open_root_dir(repo_root)?;
    let loops_fd = match open_dir_nofollow(repo_fd.as_fd(), ".loops") {
        Ok(fd) => fd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            bail!("diretório '.loops' é um link simbólico e foi recusado");
        }
        Err(err) => bail!("falha ao abrir '.loops': {err}"),
    };
    let Some(raw) = read_regular_file_at(loops_fd.as_fd(), "models.toml", MAX_MODELS_TOML_BYTES)?
    else {
        return Ok(None);
    };
    Ok(Some(parse_project_models(&raw)?))
}

pub fn resolve_workflow(
    unresolved: UnresolvedWorkflow,
    workflow_origin: WorkflowOrigin,
    use_flags: &[(String, ModelChoice)],
    project: Option<&ProjectModels>,
    global_defaults: &RoleDefaults,
) -> std::result::Result<WorkflowResolution, Vec<String>> {
    let mut errors = Vec::new();
    let known_stage_ids: BTreeSet<&str> = unresolved.stages.iter().map(|s| s.id.as_str()).collect();
    let mut use_map: BTreeMap<&str, &ModelChoice> = BTreeMap::new();
    for (stage_id, choice) in use_flags {
        if !known_stage_ids.contains(stage_id.as_str()) {
            errors.push(format!(
                "etapa desconhecida em --use: '{stage_id}' no workflow '{}'",
                unresolved.name
            ));
        } else if use_map.insert(stage_id.as_str(), choice).is_some() {
            errors.push(format!("etapa '{stage_id}' repetida em --use"));
        }
    }

    let project_stages = project.and_then(|p| p.stages.get(&unresolved.name));
    let mut resolved_stages = Vec::with_capacity(unresolved.stages.len());
    let mut resolutions = Vec::with_capacity(unresolved.stages.len());

    for st in unresolved.stages {
        // Provider and model always come together from the highest winning level:
        // 1. --use stage=provider[:model]
        // 2. [stages.<workflow>.<stage>] in .loops/models.toml
        // 3. stage in workflow TOML
        // 4. [defaults.<role>] in .loops/models.toml
        // 5. [defaults.<role>] in loops.toml
        let chosen: Option<(ModelChoice, ModelSource)> =
            if let Some(choice) = use_map.get(st.id.as_str()) {
                Some(((*choice).clone(), ModelSource::RunFlag))
            } else if let Some(choice) = project_stages.and_then(|m| m.get(&st.id)) {
                Some((choice.clone(), ModelSource::ProjectStage))
            } else if let Some(provider) = st.provider {
                Some((
                    ModelChoice {
                        provider,
                        model: st.model.clone(),
                    },
                    ModelSource::Workflow,
                ))
            } else if let Some(RoleDefault { provider, model }) =
                project.and_then(|p| p.defaults.get(&st.role))
            {
                Some((
                    ModelChoice {
                        provider: *provider,
                        model: model.clone(),
                    },
                    ModelSource::ProjectDefault,
                ))
            } else {
                global_defaults.get(&st.role).map(|def| {
                    (
                        ModelChoice {
                            provider: def.provider,
                            model: def.model.clone(),
                        },
                        ModelSource::GlobalDefault,
                    )
                })
            };

        let Some((choice, source)) = chosen else {
            errors.push(format!(
                "etapa '{}': provider ausente e sem padrão em [defaults.{}] para o role '{}'",
                st.id, st.role, st.role
            ));
            continue;
        };

        let from_repo = source.is_from_repo(workflow_origin);
        resolutions.push(StageResolution {
            stage_id: st.id.clone(),
            role: st.role.clone(),
            choice: choice.clone(),
            source,
            from_repo,
        });
        resolved_stages.push(Stage {
            id: st.id,
            role: st.role,
            provider: choice.provider,
            model: choice.model,
            reads: st.reads,
            writes: st.writes,
            edits: st.edits,
            next: st.next,
            verdict_gate: st.verdict_gate,
            on_pass: st.on_pass,
            on_retry: st.on_retry,
        });
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(WorkflowResolution {
        workflow: Workflow {
            name: unresolved.name,
            description: unresolved.description,
            max_cycles: unresolved.max_cycles,
            stage_timeout_secs: unresolved.stage_timeout_secs,
            stall_secs: unresolved.stall_secs,
            max_duration_secs: unresolved.max_duration_secs,
            stages: resolved_stages,
        },
        stages: resolutions,
    })
}

pub fn check_project_models_references(
    project: &ProjectModels,
    known_workflows: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (wf_name, stages_map) in &project.stages {
        let Some(valid_stages) = known_workflows.get(wf_name) else {
            warnings.push(format!(
                ".loops/models.toml referencia workflow desconhecido '{wf_name}'"
            ));
            continue;
        };
        for stage_id in stages_map.keys() {
            if !valid_stages.iter().any(|s| s == stage_id) {
                warnings.push(format!(
                    ".loops/models.toml referencia etapa desconhecida '{stage_id}' no workflow '{wf_name}'"
                ));
            }
        }
    }
    warnings
}

pub fn set_default_in_toml(raw: &str, role: &str, choice: &ModelChoice) -> Result<String> {
    validate_key_segment(role, &format!("[defaults.{role}]"))?;
    if let Some(m) = &choice.model {
        validate_model_name(m).map_err(|e| anyhow::anyhow!("[defaults.{role}]: {e}"))?;
    }

    let mut doc = raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        let start = e.span().map_or(0, |span| span.start);
        let line = line_at(raw, start);
        anyhow::anyhow!("TOML inválido na linha {line}: {}", e.message().trim())
    })?;

    if !doc.contains_key("defaults") {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        doc["defaults"] = toml_edit::Item::Table(t);
    }
    let Some(defaults_table) = doc["defaults"].as_table_mut() else {
        bail!("campo 'defaults' deve ser uma tabela TOML");
    };

    if !defaults_table.contains_key(role) {
        defaults_table.insert(role, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let Some(role_table) = defaults_table[role].as_table_mut() else {
        bail!("campo 'defaults.{role}' deve ser uma tabela TOML");
    };

    role_table["provider"] = toml_edit::value(choice.provider.as_str());
    match &choice.model {
        Some(m) => role_table["model"] = toml_edit::value(m.as_str()),
        None => {
            role_table.remove("model");
        }
    }

    Ok(doc.to_string())
}

fn ensure_no_symlink_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!(
                    "caminho '{}' é um link simbólico e foi recusado",
                    path.display()
                );
            }
            if !meta.file_type().is_file() {
                bail!("caminho '{}' não é um arquivo regular", path.display());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            bail!("falha ao inspecionar '{}': {err}", path.display());
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(meta) = fs::symlink_metadata(parent) {
                if meta.file_type().is_symlink() {
                    bail!(
                        "diretório '{}' é um link simbólico e foi recusado",
                        parent.display()
                    );
                }
            }
        }
    }
    Ok(())
}

fn write_atomic_file(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    ensure_no_symlink_file(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("falha ao criar diretório '{}'", parent.display()))?;
    ensure_no_symlink_file(path)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("nome de arquivo inválido: {}", path.display()))?;
    let tmp_name = format!(".{file_name}.tmp.{}", std::process::id());
    let tmp_path = parent.join(&tmp_name);
    let _ = fs::remove_file(&tmp_path);

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    if let Some(m) = mode {
        opts.mode(m);
    }
    let mut file = opts
        .open(&tmp_path)
        .with_context(|| format!("falha ao criar arquivo temporário '{}'", tmp_path.display()))?;

    let write_res = (|| -> Result<()> {
        if let Some(m) = mode {
            // SAFETY: file descriptor is valid; enforce exact mode independent of umask.
            if unsafe { libc::fchmod(file.as_raw_fd(), m as libc::mode_t) } != 0 {
                return Err(std::io::Error::last_os_error()).context("fchmod falhou");
            }
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_res {
        drop(file);
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    drop(file);

    ensure_no_symlink_file(path)?;
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e).with_context(|| {
            format!(
                "falha ao renomear '{}' para '{}'",
                tmp_path.display(),
                path.display()
            )
        });
    }
    if let Ok(dir_handle) = File::open(parent) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

pub fn write_default_in_toml_file(
    path: &Path,
    role: &str,
    choice: &ModelChoice,
    is_project: bool,
) -> Result<()> {
    ensure_no_symlink_file(path)?;
    let existing = if path.exists() {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let dir_fd = open_root_dir(parent)?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .with_context(|| format!("nome de arquivo inválido: {}", path.display()))?;
        read_regular_file_at(dir_fd.as_fd(), file_name, MAX_MODELS_TOML_BYTES)?.unwrap_or_default()
    } else {
        String::new()
    };

    let updated = set_default_in_toml(&existing, role, choice)?;
    if is_project {
        parse_project_models(&updated)?;
    } else {
        parse_loops_config(&updated)?;
    }

    // The file may be private: replacing it must not widen its mode.
    let mode = fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777);
    write_atomic_file(path, updated.as_bytes(), mode)
}

pub fn repo_choices_digest(resolution: &WorkflowResolution) -> Option<String> {
    let mut repo_stages: Vec<&StageResolution> =
        resolution.stages.iter().filter(|s| s.from_repo).collect();
    if repo_stages.is_empty() {
        return None;
    }
    repo_stages.sort_by(|a, b| a.stage_id.cmp(&b.stage_id));

    let mut hasher = Sha256::new();
    hasher.update(resolution.workflow.name.as_bytes());
    hasher.update(b"\0");
    for st in repo_stages {
        hasher.update(st.stage_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(st.choice.provider.as_str().as_bytes());
        hasher.update(b"\0");
        if let Some(m) = &st.choice.model {
            hasher.update(m.as_bytes());
        }
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    Some(hex)
}

type ConfirmedStore = BTreeMap<String, BTreeMap<String, String>>;

fn read_confirmed_store(confirmed_file: &Path) -> Result<ConfirmedStore> {
    ensure_no_symlink_file(confirmed_file)?;
    let parent = match confirmed_file.parent() {
        Some(p) if !p.as_os_str().is_empty() && p.exists() => p,
        _ => return Ok(ConfirmedStore::new()),
    };
    let dir_fd = open_root_dir(parent)?;
    let file_name = confirmed_file
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("nome de arquivo inválido: {}", confirmed_file.display()))?;
    let Some(raw) = read_regular_file_at(dir_fd.as_fd(), file_name, MAX_CONFIRMED_JSON_BYTES)?
    else {
        return Ok(ConfirmedStore::new());
    };
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "falha ao ler arquivo de confirmação '{}'",
            confirmed_file.display()
        )
    })
}

pub fn is_confirmed(
    confirmed_file: &Path,
    repo_root: &Path,
    resolution: &WorkflowResolution,
) -> Result<bool> {
    let Some(expected) = repo_choices_digest(resolution) else {
        return Ok(true);
    };
    let canonical_repo = fs::canonicalize(repo_root)
        .with_context(|| format!("falha ao resolver caminho '{}'", repo_root.display()))?
        .to_string_lossy()
        .into_owned();
    let store = read_confirmed_store(confirmed_file)?;
    Ok(store
        .get(&canonical_repo)
        .and_then(|wf_map| wf_map.get(&resolution.workflow.name))
        .is_some_and(|saved| saved == &expected))
}

pub fn record_confirmation(
    confirmed_file: &Path,
    repo_root: &Path,
    resolution: &WorkflowResolution,
) -> Result<()> {
    let Some(digest) = repo_choices_digest(resolution) else {
        return Ok(());
    };
    let canonical_repo = fs::canonicalize(repo_root)
        .with_context(|| format!("falha ao resolver caminho '{}'", repo_root.display()))?
        .to_string_lossy()
        .into_owned();
    let mut store = read_confirmed_store(confirmed_file)?;
    store
        .entry(canonical_repo)
        .or_default()
        .insert(resolution.workflow.name.clone(), digest);
    let json = serde_json::to_vec_pretty(&store).context("falha ao serializar confirmed.json")?;
    write_atomic_file(confirmed_file, &json, Some(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::workflow::{parse_workflow_unresolved, RoleDefault};
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn wf_two_stage(
        a_provider: Option<&str>,
        a_model: Option<&str>,
        b_provider: Option<&str>,
    ) -> String {
        let a_prov = a_provider
            .map(|p| format!("provider = \"{p}\"\n"))
            .unwrap_or_default();
        let a_mod = a_model
            .map(|m| format!("model = \"{m}\"\n"))
            .unwrap_or_default();
        let b_prov = b_provider
            .map(|p| format!("provider = \"{p}\"\n"))
            .unwrap_or_default();
        format!(
            r#"name = "w"
[[stages]]
id = "implement"
role = "worker"
{a_prov}{a_mod}edits = true
next = "audit"

[[stages]]
id = "audit"
role = "critic"
{b_prov}writes = "critic.md"
verdict_gate = true
on_pass = "done"
on_retry = "implement"
"#
        )
    }

    #[test]
    fn parse_use_flag_preserves_slashes_and_colons_and_rejects_bad_syntax() {
        let (stage, ch) =
            parse_use_flag("implement=opencode:anthropic/claude-3-7:beta:free").unwrap();
        assert_eq!(stage, "implement");
        assert_eq!(ch.provider, Provider::Opencode);
        assert_eq!(ch.model.as_deref(), Some("anthropic/claude-3-7:beta:free"));

        let (stage, ch) = parse_use_flag("audit=claude").unwrap();
        assert_eq!(stage, "audit");
        assert_eq!(ch.provider, Provider::Claude);
        assert_eq!(ch.model, None);

        for bad in [
            "implement",
            "=claude",
            "implement=",
            "implement=unknown",
            "implement=claude:",
            "implement=claude:   ",
            "../x=claude",
        ] {
            assert!(parse_use_flag(bad).is_err(), "expected error for {bad:?}");
        }
    }

    #[test]
    fn parse_project_models_rejects_unknown_keys_sandbox_and_model_without_provider() {
        let valid = r#"
[defaults.worker]
provider = "claude"
model = "claude-sonnet-4"

[stages.w.implement]
provider = "opencode"
model = "zai/glm-5.3-flash"
"#;
        let parsed = parse_project_models(valid).unwrap();
        assert_eq!(parsed.defaults["worker"].provider, Provider::Claude);
        assert_eq!(
            parsed.stages["w"]["implement"],
            ModelChoice {
                provider: Provider::Opencode,
                model: Some("zai/glm-5.3-flash".to_string()),
            }
        );

        let with_sandbox = r#"
[sandbox]
kind = "inherited"
"#;
        assert!(parse_project_models(with_sandbox).is_err());

        let with_unknown = r#"
extra = true
[defaults.worker]
provider = "claude"
"#;
        assert!(parse_project_models(with_unknown).is_err());

        let with_stage_extra = r#"
[stages.w.implement]
provider = "claude"
extra = "x"
"#;
        assert!(parse_project_models(with_stage_extra).is_err());

        let default_no_prov = r#"
[defaults.worker]
model = "only-model"
"#;
        let err = parse_project_models(default_no_prov).unwrap_err();
        assert!(err.to_string().contains("provider"), "got: {err}");

        let stage_no_prov = r#"
[stages.w.implement]
model = "only-model"
"#;
        let err = parse_project_models(stage_no_prov).unwrap_err();
        assert!(err.to_string().contains("provider"), "got: {err}");
    }

    #[test]
    fn precedence_beats_lower_levels_without_mixing_provider_and_model() {
        let mut global = RoleDefaults::new();
        global.insert(
            "worker".into(),
            RoleDefault {
                provider: Provider::Agy,
                model: Some("global-model".into()),
            },
        );
        global.insert(
            "critic".into(),
            RoleDefault {
                provider: Provider::Claude,
                model: Some("global-critic".into()),
            },
        );

        // Level 5 (GlobalDefault) alone when nothing above is set.
        let unres = parse_workflow_unresolved(&wf_two_stage(None, None, None)).unwrap();
        let res = resolve_workflow(unres, WorkflowOrigin::Preset, &[], None, &global).unwrap();
        assert_eq!(res.workflow.stages[0].provider, Provider::Agy);
        assert_eq!(
            res.workflow.stages[0].model.as_deref(),
            Some("global-model")
        );
        assert_eq!(res.stages[0].source, ModelSource::GlobalDefault);
        assert!(!res.needs_confirmation());

        // Level 4 (ProjectDefault) beats Level 5 (GlobalDefault) and clears global model if omitted.
        let proj_def = parse_project_models(
            r#"
[defaults.worker]
provider = "claude"
"#,
        )
        .unwrap();
        let unres = parse_workflow_unresolved(&wf_two_stage(None, None, None)).unwrap();
        let res =
            resolve_workflow(unres, WorkflowOrigin::Preset, &[], Some(&proj_def), &global).unwrap();
        assert_eq!(res.workflow.stages[0].provider, Provider::Claude);
        assert_eq!(res.workflow.stages[0].model, None);
        assert_eq!(res.stages[0].source, ModelSource::ProjectDefault);

        // Level 3 (Workflow) beats Level 4 (ProjectDefault), without mixing model.
        let proj_def_with_model = parse_project_models(
            r#"
[defaults.worker]
provider = "claude"
model = "proj-def-model"
"#,
        )
        .unwrap();
        let unres = parse_workflow_unresolved(&wf_two_stage(Some("opencode"), None, None)).unwrap();
        let res = resolve_workflow(
            unres,
            WorkflowOrigin::Repo,
            &[],
            Some(&proj_def_with_model),
            &global,
        )
        .unwrap();
        assert_eq!(res.workflow.stages[0].provider, Provider::Opencode);
        assert_eq!(res.workflow.stages[0].model, None);
        assert_eq!(res.stages[0].source, ModelSource::Workflow);

        // Level 2 (ProjectStage) beats Level 3 (Workflow), without mixing model.
        let proj_stage = parse_project_models(
            r#"
[stages.w.implement]
provider = "agy"
"#,
        )
        .unwrap();
        let unres =
            parse_workflow_unresolved(&wf_two_stage(Some("claude"), Some("wf-model"), None))
                .unwrap();
        let res =
            resolve_workflow(unres, WorkflowOrigin::Repo, &[], Some(&proj_stage), &global).unwrap();
        assert_eq!(res.workflow.stages[0].provider, Provider::Agy);
        assert_eq!(res.workflow.stages[0].model, None);
        assert_eq!(res.stages[0].source, ModelSource::ProjectStage);

        // Level 1 (RunFlag --use) beats Level 2 (ProjectStage), without mixing model.
        let proj_stage_with_model = parse_project_models(
            r#"
[stages.w.implement]
provider = "opencode"
model = "stage-model"
"#,
        )
        .unwrap();
        let unres =
            parse_workflow_unresolved(&wf_two_stage(Some("claude"), Some("wf-model"), None))
                .unwrap();
        let use_flags = vec![parse_use_flag("implement=opencode").unwrap()];
        let res = resolve_workflow(
            unres,
            WorkflowOrigin::Repo,
            &use_flags,
            Some(&proj_stage_with_model),
            &global,
        )
        .unwrap();
        assert_eq!(res.workflow.stages[0].provider, Provider::Opencode);
        assert_eq!(res.workflow.stages[0].model, None);
        assert_eq!(res.stages[0].source, ModelSource::RunFlag);
    }

    #[test]
    fn resolve_rejects_unknown_use_stage_and_missing_provider() {
        let unres = parse_workflow_unresolved(&wf_two_stage(None, None, Some("claude"))).unwrap();
        let errs = resolve_workflow(
            unres,
            WorkflowOrigin::Preset,
            &[parse_use_flag("ghost=claude").unwrap()],
            None,
            &RoleDefaults::new(),
        )
        .unwrap_err();
        let joined = errs.join("\n");
        assert!(joined.contains("ghost"), "got: {joined}");
        assert!(joined.contains("implement"), "got: {joined}");
    }

    #[test]
    fn check_reports_unknown_workflow_and_unknown_stage_in_project_models() {
        let proj = parse_project_models(
            r#"
[stages.unknown-wf.implement]
provider = "claude"

[stages.w.missing-stage]
provider = "agy"

[stages.w.implement]
provider = "opencode"
"#,
        )
        .unwrap();
        let mut known = BTreeMap::new();
        known.insert("w".into(), vec!["implement".into(), "audit".into()]);

        let warnings = check_project_models_references(&proj, &known);
        let joined = warnings.join("\n");
        assert!(joined.contains("unknown-wf"), "got: {joined}");
        assert!(joined.contains("missing-stage"), "got: {joined}");
        assert!(!joined.contains("'implement'"), "got: {joined}");
    }

    #[test]
    fn set_preserves_comments_and_other_keys_and_refuses_symlinks() {
        let raw = r#"# Cabeçalho importante
[defaults.critic]
provider = "claude"
# Comentário interno

[defaults.worker]
provider = "agy"
model = "old-model"

[stages.w.implement]
provider = "opencode"
"#;
        let updated = set_default_in_toml(
            raw,
            "worker",
            &ModelChoice {
                provider: Provider::Opencode,
                model: Some("zai/glm-5.3-flash".into()),
            },
        )
        .unwrap();
        assert!(updated.contains("# Cabeçalho importante"));
        assert!(updated.contains("# Comentário interno"));
        assert!(updated.contains("[stages.w.implement]"));
        let reparsed = parse_project_models(&updated).unwrap();
        assert_eq!(reparsed.defaults["worker"].provider, Provider::Opencode);
        assert_eq!(
            reparsed.defaults["worker"].model.as_deref(),
            Some("zai/glm-5.3-flash")
        );

        // Removing model when setting provider without model:
        let cleared = set_default_in_toml(
            &updated,
            "worker",
            &ModelChoice {
                provider: Provider::Claude,
                model: None,
            },
        )
        .unwrap();
        let reparsed2 = parse_project_models(&cleared).unwrap();
        assert_eq!(reparsed2.defaults["worker"].provider, Provider::Claude);
        assert_eq!(reparsed2.defaults["worker"].model, None);

        // Symlink refusal
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("real.toml");
        std::fs::write(&target, "").unwrap();
        let link = tmp.path().join("models.toml");
        symlink(&target, &link).unwrap();
        let err = write_default_in_toml_file(
            &link,
            "worker",
            &ModelChoice {
                provider: Provider::Claude,
                model: None,
            },
            true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("simbólico"), "got: {err}");
    }

    #[test]
    fn load_project_models_reads_repo_only_ignores_workspace_and_refuses_symlink() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(repo.join(".loops")).unwrap();
        std::fs::create_dir_all(ws.join(".loops")).unwrap();

        std::fs::write(
            repo.join(".loops/models.toml"),
            "[defaults.worker]\nprovider = \"claude\"\n",
        )
        .unwrap();
        // Poisoned workspace models.toml must be ignored when loading from repo root.
        std::fs::write(
            ws.join(".loops/models.toml"),
            "[defaults.worker]\nprovider = \"agy\"\n",
        )
        .unwrap();

        let loaded = load_project_models_from_repo(&repo).unwrap().unwrap();
        assert_eq!(loaded.defaults["worker"].provider, Provider::Claude);

        // Symlinked models.toml is refused.
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "[defaults.worker]\nprovider = \"agy\"\n").unwrap();
        std::fs::remove_file(repo.join(".loops/models.toml")).unwrap();
        symlink(&outside, repo.join(".loops/models.toml")).unwrap();
        assert!(load_project_models_from_repo(&repo).is_err());
    }

    #[test]
    fn confirmation_tracks_repo_choices_0600_and_skips_use_or_global_only() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let confirmed_path = tmp.path().join("state/tchelvis/loops/confirmed.json");

        let mut global = RoleDefaults::new();
        global.insert(
            "worker".into(),
            RoleDefault {
                provider: Provider::Claude,
                model: None,
            },
        );
        global.insert(
            "critic".into(),
            RoleDefault {
                provider: Provider::Claude,
                model: None,
            },
        );

        // Global/preset only -> asks nothing.
        let res_global = resolve_workflow(
            parse_workflow_unresolved(&wf_two_stage(None, None, None)).unwrap(),
            WorkflowOrigin::Preset,
            &[],
            None,
            &global,
        )
        .unwrap();
        assert!(!res_global.needs_confirmation());
        assert!(is_confirmed(&confirmed_path, &repo, &res_global).unwrap());
        assert!(!confirmed_path.exists());

        // Repo choice -> needs confirmation.
        let proj = parse_project_models(
            "[defaults.worker]\nprovider = \"opencode\"\nmodel = \"zai/glm-5.3-flash\"\n",
        )
        .unwrap();
        let res_repo = resolve_workflow(
            parse_workflow_unresolved(&wf_two_stage(None, None, None)).unwrap(),
            WorkflowOrigin::Preset,
            &[],
            Some(&proj),
            &global,
        )
        .unwrap();
        assert!(res_repo.needs_confirmation());
        assert!(!is_confirmed(&confirmed_path, &repo, &res_repo).unwrap());

        // Overridden completely with --use -> asks nothing.
        let res_use = resolve_workflow(
            parse_workflow_unresolved(&wf_two_stage(None, None, None)).unwrap(),
            WorkflowOrigin::Repo,
            &[
                parse_use_flag("implement=claude").unwrap(),
                parse_use_flag("audit=agy").unwrap(),
            ],
            Some(&proj),
            &global,
        )
        .unwrap();
        assert!(!res_use.needs_confirmation());
        assert!(is_confirmed(&confirmed_path, &repo, &res_use).unwrap());

        // Record confirmation for repo choice -> creates 0600 file and validates.
        record_confirmation(&confirmed_path, &repo, &res_repo).unwrap();
        let mode = std::fs::metadata(&confirmed_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(is_confirmed(&confirmed_path, &repo, &res_repo).unwrap());

        // Changing repo choice invalidates previous confirmation.
        let proj2 = parse_project_models(
            "[defaults.worker]\nprovider = \"opencode\"\nmodel = \"other-model\"\n",
        )
        .unwrap();
        let res_repo2 = resolve_workflow(
            parse_workflow_unresolved(&wf_two_stage(None, None, None)).unwrap(),
            WorkflowOrigin::Preset,
            &[],
            Some(&proj2),
            &global,
        )
        .unwrap();
        assert!(!is_confirmed(&confirmed_path, &repo, &res_repo2).unwrap());
    }

    #[test]
    fn model_names_that_could_be_options_or_break_lines_are_refused_everywhere() {
        for bad in [
            "-x",
            "--dangerously-skip",
            " a",
            "a ",
            "a\\nb",
            "a\\u0000b",
            "a\\u001bb",
        ] {
            let project = format!("[defaults.worker]\nprovider = \"claude\"\nmodel = \"{bad}\"\n");
            assert!(parse_project_models(&project).is_err(), "{bad}");
            let stage = format!("[stages.w.a]\nprovider = \"claude\"\nmodel = \"{bad}\"\n");
            assert!(parse_project_models(&stage).is_err(), "{bad}");
            let global = format!("[defaults.worker]\nprovider = \"claude\"\nmodel = \"{bad}\"\n");
            assert!(parse_loops_config(&global).is_err(), "{bad}");
            let workflow = wf_two_stage(Some("claude"), Some(bad), Some("claude"));
            assert!(parse_workflow_unresolved(&workflow).is_err(), "{bad}");
        }
        for bad in ["a=claude:-x", "a=claude: x", "a=claude:x\ny"] {
            assert!(parse_use_flag(bad).is_err(), "{bad}");
        }
        for good in [
            "opencode-go/hy4-flash-free",
            "zai/glm-5.3:free",
            "GPT-OSS 120B (Medium)",
        ] {
            assert!(
                parse_use_flag(&format!("a=opencode:{good}")).is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn the_same_stage_twice_in_use_is_an_error() {
        let unresolved =
            parse_workflow_unresolved(&wf_two_stage(Some("claude"), None, Some("claude"))).unwrap();
        let flag = |raw: &str| parse_use_flag(raw).unwrap();
        let errors = resolve_workflow(
            unresolved,
            WorkflowOrigin::Global,
            &[flag("implement=agy"), flag("implement=claude")],
            None,
            &RoleDefaults::new(),
        )
        .unwrap_err();
        assert!(errors.join(" ").contains("implement"), "{errors:?}");
    }

    #[test]
    fn set_keeps_the_mode_of_the_file_and_errors_never_quote_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("loops.toml");
        fs::write(&path, "max_concurrent = 1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let choice = ModelChoice {
            provider: Provider::Claude,
            model: None,
        };
        write_default_in_toml_file(&path, "worker", &choice, false).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(&path, "segredo_da_linha = oops oops\n").unwrap();
        let err = format!(
            "{:#}",
            write_default_in_toml_file(&path, "worker", &choice, false).unwrap_err()
        );
        assert!(!err.contains("segredo_da_linha"), "{err}");
    }

    #[test]
    fn project_models_refuses_preflight_section() {
        let raw = r#"
[preflight]
mode = "free"
valid_hours = 1
"#;
        assert!(parse_project_models(raw).is_err());
    }
}
