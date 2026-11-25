use crate::config::CockpitPaths;
use crate::herdr::HerdrClient;
use crate::loops::config::{
    check_home_ro_does_not_reach, parse_loops_config, LoopsConfig, PreflightConfig, PreflightMode,
    SandboxConfig, DEFAULT_MAX_CONCURRENT,
};
use crate::loops::engine::{Engine, EngineConfig, EvidenceRecorder, ProcessStageRunner};
use crate::loops::evidence::{
    compute_account_identity, compute_executable_identity, hex_decode_32, hex_encode,
    AccountIdentityInput, CliIdentity, EvidenceStore,
};
use crate::loops::free_check::{
    is_model_listed, plan_preflight, run_free_checks_with_options, ClaudeStatus, FreeCheck, Listed,
    PiAccount, PlanAction, PlannedCheck, ProviderFreeOutcome, AGY_ABSENT_REASON,
};
use crate::loops::models::{
    check_project_models_references, is_confirmed, load_project_models_from_repo, parse_use_flag,
    record_confirmation, resolve_workflow, write_default_in_toml_file, ModelChoice, ModelSource,
    ProjectModels, WorkflowOrigin,
};
use crate::loops::preflight::{
    probe_targets, run_preflight_with_options, ProbeFailure, ProbeMethod, ProbeRecord, ProbeTarget,
};
use crate::loops::process::SystemClock;
use crate::loops::report::render_report_with_secrets;
use crate::loops::sandbox::{
    pi_managed_install, resolve_agy_credential, AgyCredentialSource, BwrapSandbox,
    InheritedSandbox, PiInstall, Sandbox, AGY_TOKEN_FILE,
};
use crate::loops::state::{
    format_result, load_state, redact_loop_text, sanitize_single_line, CallerInfo, RunState, Usage,
};
use crate::loops::watch::{
    current_process_identity, LinuxProcTable, ProcTable, ENGINE_START_GRACE_MS,
};
use crate::loops::workflow::{
    is_valid_role_name, is_valid_slug, is_valid_stage_id, parse_workflow_unresolved, Provider,
    RoleDefaults, UnresolvedWorkflow, Workflow, DEFAULT_MAX_CYCLES, DEFAULT_MAX_DURATION_SECS,
    DEFAULT_ROLES, DEFAULT_STAGE_TIMEOUT_SECS, DEFAULT_STALL_SECS, PRESET_WORKFLOWS,
};
use crate::loops::workspace::{FrozenWorkspaceMeta, GitWorkspace};
use crate::model::{IpcOp, IpcRequest};
use crate::orch::store::{LoopRunRecord, OrchStore};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const EXIT_PASS: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_ESCALATE: i32 = 3;
pub const EXIT_CANCELLED: i32 = 4;

const MAX_INPUT_FILE_BYTES: usize = 1024 * 1024;
const MAX_WORKFLOW_TOML_BYTES: usize = 256 * 1024;
const MAX_ROLE_MD_BYTES: usize = 1024 * 1024;
const MAX_FROZEN_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOG_PRINT_BYTES: usize = 64 * 1024 * 1024;
const OPENCODE_KEY_START: &str = "\x1eTCHELVIS_OPENCODE_KEY_START\x1e";
const OPENCODE_KEY_END: &str = "\x1eTCHELVIS_OPENCODE_KEY_END\x1e";
const OPENCODE_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Subcommand)]
pub enum LoopCommands {
    /// Inicializa .loops/ e ~/.config/tchelvis/loops.toml (preserva arquivos existentes)
    Init,
    /// Cria um novo workflow em .loops/workflows/<nome>.toml
    New {
        /// Nome do workflow ([a-z0-9-]+)
        name: String,
        /// Lista separada por vírgulas de roles das etapas em ordem
        #[arg(long, required = true, value_delimiter = ',')]
        stages: Vec<String>,
    },
    /// Lista workflows disponíveis ou execuções registradas (--runs / --active)
    List {
        /// Lista execuções registradas em vez de workflows
        #[arg(long)]
        runs: bool,
        /// Lista apenas execuções ativas no momento
        #[arg(long)]
        active: bool,
    },
    /// Valida workflow, configuração, modelos e sandbox (sem gastar tokens, salvo --live)
    Check {
        /// Nome do workflow
        workflow: String,
        /// Executa também o pré-teste real nos modelos do workflow
        #[arg(long)]
        live: bool,
        /// Sobrescreve CLI/modelo de uma etapa (etapa=cli[:modelo])
        #[arg(long = "use")]
        use_overrides: Vec<String>,
    },
    /// Inspeciona ou configura modelos por workflow e role
    #[command(args_conflicts_with_subcommands = true)]
    Models {
        #[command(subcommand)]
        action: Option<ModelsSubcommand>,
        /// Nome do workflow (`tchelvis loop models <workflow>`)
        workflow: Option<String>,
    },
    /// Verifica contas dos CLIs ou abre login oficial via Herdr
    #[command(args_conflicts_with_subcommands = true)]
    Auth {
        #[command(subcommand)]
        action: Option<AuthSubcommand>,
    },
    /// Executa um workflow multi-agente em sandbox
    Run {
        /// Nome do workflow
        workflow: String,
        /// Tarefa/prompt (mutuamente exclusivo com --input-file)
        prompt: Option<String>,
        /// Lê a tarefa/prompt de um arquivo (mutuamente exclusivo com prompt)
        #[arg(long, conflicts_with = "prompt")]
        input_file: Option<PathBuf>,
        /// Parte do commit HEAD mesmo com mudanças não commitadas
        #[arg(long)]
        from_head: bool,
        /// Número máximo de ciclos (1..=20)
        #[arg(long)]
        max_cycles: Option<u32>,
        /// Sobrescreve CLI/modelo de uma etapa (etapa=cli[:modelo])
        #[arg(long = "use")]
        use_overrides: Vec<String>,
        /// Inicia o motor em um terminal Herdr e retorna apenas o RUN_ID
        #[arg(long)]
        detach: bool,
        /// Exibe detalhes extras de progresso durante a execução
        #[arg(long)]
        verbose: bool,
    },
    /// Mostra o status de uma execução
    Status {
        /// ID da execução (opcional; usa a mais recente se omitido)
        run_id: Option<String>,
        /// Saída em JSON
        #[arg(long)]
        json: bool,
    },
    /// Imprime a saída resumida de 5 linhas de uma execução
    Result {
        /// ID da execução (opcional; usa a mais recente se omitido)
        run_id: Option<String>,
    },
    /// Exibe os logs de uma execução
    Logs {
        /// ID da execução (opcional; usa a mais recente se omitido)
        run_id: Option<String>,
        /// Filtra por ID do estágio
        #[arg(long)]
        stage: Option<String>,
    },
    /// Exibe o report.md gerado para uma execução
    Report {
        /// ID da execução (opcional; usa a mais recente se omitido)
        run_id: Option<String>,
    },
    /// Solicita o cancelamento de uma execução em andamento (SIGINT ao motor)
    Stop {
        /// ID da execução (opcional; sem ID, usa a única execução ativa do repositório atual)
        run_id: Option<String>,
    },
    /// Remove artefatos de disco e workspaces de execuções finalizadas (mantém branches)
    Clean {
        /// ID específico para limpar (opcional; limpa todas finalizadas se omitido)
        run_id: Option<String>,
        /// Confirma sem pedir confirmação interativa
        #[arg(long)]
        yes: bool,
    },
    /// Ponto de entrada interno para execução destacada via Herdr
    #[command(hide = true)]
    Engine {
        /// ID da execução previamente preparada
        run_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelsSubcommand {
    /// Define o padrão de um role em .loops/models.toml (--project) ou ~/.config/tchelvis/loops.toml (--global)
    Set {
        /// Especificação role=cli[:modelo]
        spec: String,
        /// Grava em <repo>/.loops/models.toml
        #[arg(long, conflicts_with = "global")]
        project: bool,
        /// Grava em ~/.config/tchelvis/loops.toml
        #[arg(long, conflicts_with = "project")]
        global: bool,
    },
    /// Registra confirmação para os modelos vindos do repositório neste workflow
    Confirm {
        /// Nome do workflow
        workflow: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthSubcommand {
    /// Abre uma sessão de login oficial em um terminal Herdr para <cli> (claude|agy|opencode|pi)
    Login {
        /// CLI alvo (claude|agy|opencode|pi)
        cli: String,
        /// Provedor do 'pi' (ex.: opencode-go); exigido só com cli = pi
        #[arg(value_name = "provedor")]
        provider: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct FrozenRunConfig {
    run_id: String,
    workflow: Workflow,
    roles: BTreeMap<String, String>,
    input_prompt: String,
    sandbox_kind: String,
    sandbox: SandboxConfig,
    env_pass: Vec<String>,
    sandbox_exposures: Vec<String>,
    branch: String,
    base_commit: Option<String>,
    caller: Option<CallerInfo>,
    model_sources: BTreeMap<String, ModelSource>,
    preflight: Vec<ProbeRecord>,
    #[serde(default)]
    evidence_identities: BTreeMap<Provider, CliIdentity>,
    #[serde(default)]
    evidence_salt: Option<String>,
    workspace: FrozenWorkspaceMeta,
    /// `PATH` the CLIs were verified with: a Herdr terminal has its own.
    path: Option<String>,
}

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn create(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(&path)
            .with_context(|| format!("falha ao criar diretório temporário {}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct FlockGuard {
    file: File,
}

impl FlockGuard {
    fn try_lock_exclusive(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("falha ao abrir trava {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                bail!("já existe um motor em execução para este loop");
            }
            bail!("falha ao adquirir trava em {}: {err}", path.display());
        }
        let (pid, ticks) = current_process_identity();
        let _ = file.set_len(0);
        let _ = writeln!(file, "{pid}:{}", ticks.unwrap_or(0));
        let _ = file.flush();
        Ok(Self { file })
    }

    fn is_locked(path: &Path) -> bool {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
            return false;
        };
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
            false
        } else {
            std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
        }
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct LoopError {
    code: i32,
    message: String,
}

impl LoopError {
    fn usage(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: msg.into(),
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_ERROR,
            message: msg.into(),
        }
    }

    fn cancelled(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_CANCELLED,
            message: msg.into(),
        }
    }
}

pub fn validate_run_id(run_id: &str) -> Result<(), String> {
    if run_id.is_empty()
        || run_id.len() > 64
        || !run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!(
            "run_id inválido '{run_id}' (use apenas [a-zA-Z0-9_-], até 64 caracteres)"
        ));
    }
    Ok(())
}

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn generate_run_id() -> String {
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let mut bytes = [0u8; 2];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bytes[0] = (nanos & 0xff) as u8;
        bytes[1] = ((nanos >> 8) ^ std::process::id()) as u8;
    }
    format!("{ts}-{:02x}{:02x}", bytes[0], bytes[1])
}

fn find_git_repo_root() -> Result<PathBuf> {
    let mut cmd = Command::new("git");
    for (name, _) in std::env::vars_os() {
        let s = name.to_string_lossy();
        if s.starts_with("GIT_")
            && !matches!(
                s.as_ref(),
                "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_NOSYSTEM"
            )
        {
            cmd.env_remove(name);
        }
    }
    cmd.args(["rev-parse", "--show-toplevel"]);
    let out = cmd.output().context("falha ao executar git rev-parse")?;
    if !out.status.success() {
        bail!("diretório atual não está dentro de um repositório git");
    }
    let root = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Ok(fs::canonicalize(&root).unwrap_or(root))
}

fn try_find_git_repo_root() -> Option<PathBuf> {
    find_git_repo_root().ok()
}

fn find_in_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(bin);
        if let Ok(meta) = fs::metadata(&cand) {
            if meta.is_file() && (meta.permissions().mode() & 0o111) != 0 {
                return Some(cand);
            }
        }
    }
    None
}

fn open_dir_nofollow_from_root(root: &Path, segments: &[&str]) -> Result<Option<OwnedFd>> {
    let root_c = CString::new(root.to_str().context("caminho inválido")?)?;
    let root_fd = unsafe {
        libc::open(
            root_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(err).with_context(|| format!("falha ao abrir {}", root.display()));
    }
    let mut cur = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for seg in segments {
        let c_seg = CString::new(*seg)?;
        let fd = unsafe {
            libc::openat(
                cur.as_raw_fd(),
                c_seg.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            if err.raw_os_error() == Some(libc::ELOOP) {
                bail!("'{seg}' é um link simbólico e foi recusado");
            }
            if err.raw_os_error() == Some(libc::ENOTDIR) {
                bail!("'{seg}' não é um diretório");
            }
            return Err(err).with_context(|| format!("falha ao abrir '{seg}'"));
        }
        cur = unsafe { OwnedFd::from_raw_fd(fd) };
    }
    Ok(Some(cur))
}

fn read_regular_file_at(
    dir: BorrowedFd<'_>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<String>> {
    let c_name = CString::new(name)?;
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
            bail!("'{name}' é um link simbólico e foi recusado");
        }
        return Err(err).with_context(|| format!("falha ao abrir '{name}'"));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao inspecionar '{name}'"));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        bail!("'{name}' não é um arquivo regular");
    }
    if st.st_size < 0 || st.st_size as usize > max_bytes {
        bail!("'{name}' excede o tamanho máximo permitido ({max_bytes} bytes)");
    }
    let file = File::from(owned);
    let mut buf = Vec::with_capacity(st.st_size as usize);
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut buf)
        .with_context(|| format!("falha ao ler '{name}'"))?;
    if buf.len() > max_bytes {
        bail!("'{name}' excede o tamanho máximo permitido ({max_bytes} bytes)");
    }
    let text = String::from_utf8(buf)
        .with_context(|| format!("'{name}' não está codificado em UTF-8 válido"))?;
    Ok(Some(text))
}

fn read_regular_file_nofollow(path: &Path, max_bytes: usize) -> Result<String> {
    String::from_utf8(read_regular_bytes_nofollow(path, max_bytes)?)
        .with_context(|| format!("'{}' não é UTF-8 válido", path.display()))
}

fn read_regular_bytes_nofollow(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    let raw_path = path.to_str().context("caminho não UTF-8")?;
    let c_path = CString::new(raw_path)?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            bail!("'{}' é um link simbólico e foi recusado", path.display());
        }
        return Err(err).with_context(|| format!("falha ao abrir '{}'", path.display()));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao inspecionar '{}'", path.display()));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        bail!("'{}' não é um arquivo regular", path.display());
    }
    if st.st_size < 0 || st.st_size as usize > max_bytes {
        bail!(
            "'{}' excede o tamanho máximo permitido ({max_bytes} bytes)",
            path.display()
        );
    }
    let file = File::from(owned);
    let mut buf = Vec::with_capacity(st.st_size as usize);
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut buf)
        .with_context(|| format!("falha ao ler '{}'", path.display()))?;
    if buf.len() > max_bytes {
        bail!(
            "'{}' excede o tamanho máximo permitido ({max_bytes} bytes)",
            path.display()
        );
    }
    Ok(buf)
}

fn ensure_dir_no_symlink(root: &Path, segments: &[&str]) -> Result<PathBuf> {
    let mut cur = root.to_path_buf();
    for seg in segments {
        cur = cur.join(seg);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    bail!("'{}' é um link simbólico e foi recusado", cur.display());
                }
                if !meta.file_type().is_dir() {
                    bail!("'{}' já existe e não é um diretório", cur.display());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cur)
                    .with_context(|| format!("falha ao criar diretório {}", cur.display()))?;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("falha ao verificar {}", cur.display()));
            }
        }
    }
    Ok(cur)
}

fn write_if_absent_nofollow(path: &Path, content: &str, mode: u32) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("'{}' é um link simbólico e foi recusado", path.display());
            }
            return Ok(false);
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("falha ao verificar {}", path.display()));
        }
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("falha ao criar {}", path.display()))?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(true)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("caminho inválido")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("nome de arquivo inválido")?;
    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp_path)
        .with_context(|| format!("falha ao criar temporário {}", tmp_path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp_path, path).with_context(|| {
        let _ = fs::remove_file(&tmp_path);
        format!("falha ao gravar {}", path.display())
    })?;
    Ok(())
}

fn load_loops_config_required(paths: &CockpitPaths) -> Result<LoopsConfig> {
    let cfg_path = paths.config_dir.join("loops.toml");
    if fs::symlink_metadata(&cfg_path).is_err() {
        bail!(
            "arquivo '{}' não encontrado (rode 'tchelvis loop init' e configure auto_approve_in_sandbox = true)",
            cfg_path.display()
        );
    }
    let raw = read_regular_file_nofollow(&cfg_path, MAX_WORKFLOW_TOML_BYTES)?;
    parse_loops_config(&raw).with_context(|| format!("erro em '{}'", cfg_path.display()))
}

fn load_loops_config_optional(paths: &CockpitPaths) -> Result<Option<LoopsConfig>> {
    let cfg_path = paths.config_dir.join("loops.toml");
    match fs::symlink_metadata(&cfg_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("falha ao verificar '{}'", cfg_path.display()))
        }
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!(
                    "'{}' é um link simbólico e foi recusado",
                    cfg_path.display()
                );
            }
            let raw = read_regular_file_nofollow(&cfg_path, MAX_WORKFLOW_TOML_BYTES)?;
            let cfg = parse_loops_config(&raw)
                .with_context(|| format!("erro em '{}'", cfg_path.display()))?;
            Ok(Some(cfg))
        }
    }
}

fn load_workflow_unresolved(
    name: &str,
    repo_root: Option<&Path>,
    paths: &CockpitPaths,
    max_cycles_override: Option<u32>,
) -> Result<(UnresolvedWorkflow, WorkflowOrigin)> {
    if !is_valid_slug(name, 48) {
        bail!("nome de workflow inválido '{name}' (use apenas [a-z0-9-]+)");
    }
    let filename = format!("{name}.toml");

    let mut found: Option<(String, WorkflowOrigin)> = None;
    if let Some(repo) = repo_root {
        if let Some(wf_dir) = open_dir_nofollow_from_root(repo, &[".loops", "workflows"])? {
            if let Some(raw) =
                read_regular_file_at(wf_dir.as_fd(), &filename, MAX_WORKFLOW_TOML_BYTES)?
            {
                found = Some((raw, WorkflowOrigin::Repo));
            }
        }
    }
    if found.is_none() {
        if let Some(wf_dir) =
            open_dir_nofollow_from_root(&paths.config_dir, &["loops", "workflows"])?
        {
            if let Some(raw) =
                read_regular_file_at(wf_dir.as_fd(), &filename, MAX_WORKFLOW_TOML_BYTES)?
            {
                found = Some((raw, WorkflowOrigin::Global));
            }
        }
    }
    if found.is_none() {
        if let Some((_, raw)) = PRESET_WORKFLOWS.iter().find(|(p_name, _)| *p_name == name) {
            found = Some(((*raw).to_string(), WorkflowOrigin::Preset));
        }
    }

    let Some((raw, origin)) = found else {
        bail!("workflow '{name}' não encontrado no projeto, em ~/.config/tchelvis/loops/workflows/ nem nos presets");
    };

    let mut parsed = parse_workflow_unresolved(&raw)
        .map_err(|errs| anyhow::anyhow!("workflow '{name}' inválido:\n- {}", errs.join("\n- ")))?;
    if let Some(mc) = max_cycles_override {
        if !(1..=20).contains(&mc) {
            bail!("--max-cycles deve ficar entre 1 e 20");
        }
        parsed.max_cycles = mc;
    }
    Ok((parsed, origin))
}

fn load_roles_for_workflow(
    wf: &Workflow,
    repo_root: Option<&Path>,
    paths: &CockpitPaths,
) -> Result<BTreeMap<String, String>> {
    let mut roles = BTreeMap::new();
    for st in &wf.stages {
        if roles.contains_key(&st.role) {
            continue;
        }
        if !is_valid_role_name(&st.role) {
            bail!("nome de role inválido '{}'", st.role);
        }
        let filename = format!("{}.md", st.role);
        let mut content: Option<String> = None;

        if let Some(repo) = repo_root {
            if let Some(dir) = open_dir_nofollow_from_root(repo, &[".loops", "roles"])? {
                if let Some(raw) = read_regular_file_at(dir.as_fd(), &filename, MAX_ROLE_MD_BYTES)?
                {
                    content = Some(raw);
                }
            }
        }
        if content.is_none() {
            if let Some(dir) = open_dir_nofollow_from_root(&paths.config_dir, &["loops", "roles"])?
            {
                if let Some(raw) = read_regular_file_at(dir.as_fd(), &filename, MAX_ROLE_MD_BYTES)?
                {
                    content = Some(raw);
                }
            }
        }
        if content.is_none() {
            if let Some((_, default_md)) = DEFAULT_ROLES.iter().find(|(r, _)| *r == st.role) {
                content = Some((*default_md).to_string());
            }
        }

        let Some(text) = content else {
            bail!("role '{}' não encontrado em .loops/roles/, ~/.config/tchelvis/loops/roles/ ou padrões", st.role);
        };
        roles.insert(st.role.clone(), text);
    }
    Ok(roles)
}

fn build_sandbox(cfg: &LoopsConfig, paths: &CockpitPaths) -> Box<dyn Sandbox + Sync> {
    match &cfg.sandbox {
        SandboxConfig::Inherited => Box::new(InheritedSandbox::new(cfg.env_pass.clone())),
        SandboxConfig::Bwrap { home_ro } => Box::new(bwrap_sandbox(home_ro, &cfg.env_pass, paths)),
    }
}

fn bwrap_sandbox(home_ro: &[String], env_pass: &[String], paths: &CockpitPaths) -> BwrapSandbox {
    BwrapSandbox::new(home_ro.to_vec(), env_pass.to_vec(), paths.state_dir.clone())
        .with_private_dirs(vec![paths.config_dir.clone(), paths.data_dir.clone()])
}

fn ensure_sandbox_prereqs(cfg: &LoopsConfig) -> Result<(), LoopError> {
    if matches!(cfg.sandbox, SandboxConfig::Bwrap { .. }) && find_in_path("bwrap").is_none() {
        return Err(LoopError::err(
            "bubblewrap ('bwrap') não encontrado no PATH; instale o pacote 'bubblewrap' (ex.: 'sudo apt install bubblewrap' ou 'sudo pacman -S bubblewrap') — bwrap é o sandbox padrão e nunca é substituído automaticamente",
        ));
    }
    Ok(())
}

/// `opencode-go/` always needs `OPENCODE_API_KEY`, even for its free models. `opencode/` answers
/// without one, and other providers have their own credentials: the model check decides.
pub fn needs_opencode_key(model: Option<&str>) -> bool {
    model.is_some_and(|m| m.starts_with("opencode-go/"))
}

pub fn resolve_opencode_api_key() -> Option<String> {
    let env_val = std::env::var("OPENCODE_API_KEY").ok();
    let shell = std::env::var_os("SHELL");
    resolve_opencode_api_key_with(env_val.as_deref(), shell.as_deref(), OPENCODE_SHELL_TIMEOUT)
}

pub fn resolve_opencode_api_key_with(
    env_val: Option<&str>,
    shell: Option<&std::ffi::OsStr>,
    timeout: Duration,
) -> Option<String> {
    if let Some(val) = env_val {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let shell = shell?;
    if shell.is_empty() {
        return None;
    }

    let snippet = "printf '\\036TCHELVIS_OPENCODE_KEY_START\\036%s\\036TCHELVIS_OPENCODE_KEY_END\\036' \"$OPENCODE_API_KEY\"";
    let mut cmd = Command::new(shell);
    cmd.args(["-ic", snippet])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe `setsid` runs between fork and exec so we can reap any
    // grandchildren if the interactive shell hangs.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn().ok()?;
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    // A process started by the shell files may keep the output open after the shell ends, so the
    // end of the stream cannot be waited for: the closing marker is what ends the answer.
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut seen: Vec<u8> = Vec::new();
    let mut key = None;
    while seen.len() < 1024 * 1024 {
        let Ok(chunk) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
            break;
        };
        seen.extend_from_slice(&chunk);
        let text = String::from_utf8_lossy(&seen);
        let between = text
            .split_once(OPENCODE_KEY_START)
            .and_then(|(_, rest)| rest.split_once(OPENCODE_KEY_END))
            .map(|(value, _)| value.trim().to_string());
        if let Some(value) = between {
            key = Some(value).filter(|v| !v.is_empty());
            break;
        }
    }
    // SAFETY: the shell leads its own session, created above.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
    key
}

/// The key, when some stage needs it: asking for it may start the user's shell.
fn opencode_key_for(workflow: &Workflow) -> Option<String> {
    workflow
        .stages
        .iter()
        .any(|s| s.provider == Provider::Opencode)
        .then(resolve_opencode_api_key)
        .flatten()
}

fn non_empty_regular_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && meta.len() > 0
}

fn evidence_store_path(paths: &CockpitPaths) -> PathBuf {
    paths.state_dir.join("loops/model_evidence.json")
}

fn distinct_providers(targets: &[ProbeTarget]) -> Vec<Provider> {
    let mut out = Vec::new();
    for t in targets {
        if !out.contains(&t.choice.provider) {
            out.push(t.choice.provider);
        }
    }
    out
}

/// One status line for `tchelvis loop auth`, from the free check outcome and the install probe.
pub fn pi_auth_line(
    outcome: Option<&ProviderFreeOutcome>,
    install: &Result<PiInstall, String>,
) -> String {
    if let Err(reason) = install {
        return format!("pi:       {reason}");
    }
    let Some(out) = outcome else {
        return "pi:       indeterminado ('pi --offline --list-models' indisponível)".to_string();
    };
    if out.list_known && !out.models.is_empty() {
        let mut prefixes: Vec<&str> = out
            .models
            .iter()
            .filter_map(|m| m.split_once('/').map(|(p, _)| p))
            .collect();
        prefixes.sort_unstable();
        prefixes.dedup();
        return match prefixes.as_slice() {
            [p] => format!(
                "pi:       {} modelo(s) de {p} (pi --list-models)",
                out.models.len()
            ),
            _ => format!(
                "pi:       {} modelo(s) de {} provedor(es) (pi --list-models)",
                out.models.len(),
                prefixes.len()
            ),
        };
    }
    "pi:       nenhum provedor com credencial (rode 'tchelvis loop auth login pi <provedor>')"
        .to_string()
}

/// Notes for `loop check`/`loop run`: refuses an unrecognised install and warns that a `pi` stage
/// sees every credential of the real `~/.pi/agent/auth.json`.
pub fn pi_check_notes(
    workflow: &Workflow,
    install: &Result<PiInstall, String>,
) -> Result<Vec<String>, String> {
    if !workflow.stages.iter().any(|s| s.provider == Provider::Pi) {
        return Ok(Vec::new());
    }
    if let Err(reason) = install {
        return Err(reason.clone());
    }
    Ok(vec![
        "aviso: pi: a etapa vê todas as credenciais de ~/.pi/agent/auth.json".to_string(),
    ])
}

/// Absolute argv of `loop auth login` for the CLIs that need no extra wiring. `agy` is built by
/// `cmd_auth` instead, because it needs the `bwrap` path and the loop login directory.
fn login_argv(cli: &str, provider: Option<&str>) -> Result<Vec<String>, LoopError> {
    match cli {
        "claude" | "opencode" => {
            let argv = vec![cli.to_string(), "auth".into(), "login".into()];
            if provider.is_some() {
                return Err(LoopError::usage(format!(
                    "'{cli}' não aceita provedor; use 'tchelvis loop auth login {cli}'"
                )));
            }
            Ok(argv)
        }
        "pi" => {
            let Some(provider) = provider else {
                return Err(LoopError::usage(
                    "informe o provedor: tchelvis loop auth login pi <provedor>".to_string(),
                ));
            };
            if provider.is_empty()
                || provider.chars().any(char::is_whitespace)
                || provider.starts_with('-')
            {
                return Err(LoopError::usage(format!(
                    "provedor inválido '{provider}'; use 'tchelvis loop auth login pi <provedor>'"
                )));
            }
            Ok(vec![
                "pi".to_string(),
                "auth".into(),
                "login".into(),
                provider.to_string(),
            ])
        }
        other => Err(LoopError::usage(format!(
            "CLI desconhecido '{other}' (use claude, agy, opencode ou pi)"
        ))),
    }
}

fn pi_providers_of(targets: &[ProbeTarget]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in targets.iter().filter(|t| t.choice.provider == Provider::Pi) {
        let prefix = t
            .choice
            .model
            .as_deref()
            .and_then(|m| m.split_once('/'))
            .map(|(p, _)| p.to_string());
        if let Some(p) = prefix {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn build_cli_identity_for_provider(
    provider: Provider,
    free_outcomes: &BTreeMap<Provider, ProviderFreeOutcome>,
    home_dir: &Path,
    agy_state_dir: Option<&Path>,
    opencode_api_key: Option<&str>,
    sandbox_kind: &str,
    salt: &[u8; 32],
    path_var: Option<&std::ffi::OsStr>,
) -> Option<CliIdentity> {
    let executable = compute_executable_identity(provider.as_str(), path_var);
    let account = match provider {
        Provider::Claude => match free_outcomes
            .get(&Provider::Claude)
            .and_then(|o| o.claude_status.as_ref())
        {
            Some(ClaudeStatus::LoggedIn {
                email,
                org_id,
                org_name,
                login_expires_at,
            }) if org_id.is_some() || email.is_some() || login_expires_at.is_some() => {
                compute_account_identity(
                    salt,
                    AccountIdentityInput::ClaudeKnown {
                        org_id: org_id.as_deref(),
                        org_name: org_name.as_deref(),
                        email: email.as_deref(),
                        login_expires_at: login_expires_at.as_deref(),
                    },
                )
            }
            // Inside the sandbox some logins report nothing that tells accounts apart. A new login
            // rewrites the credential file, and so does a renewal, which costs one more probe.
            Some(ClaudeStatus::LoggedIn { .. }) => {
                let meta = fs::metadata(home_dir.join(".claude/.credentials.json"))
                    .ok()
                    .filter(|meta| meta.is_file() && meta.len() > 0)?;
                compute_account_identity(salt, AccountIdentityInput::ClaudeCredentialFile(&meta))
            }
            // No known account: evidence would be shared by every account.
            _ => return None,
        },
        Provider::Opencode => {
            compute_account_identity(salt, AccountIdentityInput::OpencodeKey(opencode_api_key))
        }
        Provider::Pi => {
            let install = pi_managed_install(home_dir, path_var).ok()?;
            let meta = fs::metadata(home_dir.join(".pi/agent/auth.json"))
                .ok()
                .filter(|meta| meta.is_file())?;
            return Some(CliIdentity {
                executable: format!(
                    "{}:{}",
                    compute_executable_identity(Provider::Pi.as_str(), path_var),
                    install.version
                ),
                account: compute_account_identity(salt, AccountIdentityInput::PiAuthFile(&meta)),
                sandbox: sandbox_kind.to_string(),
            });
        }
        Provider::Agy => {
            let cred = resolve_agy_credential(home_dir, agy_state_dir);
            match cred {
                Some(cred) if cred.source == AgyCredentialSource::LoopLogin => {
                    let meta = fs::symlink_metadata(&cred.path).ok()?;
                    compute_account_identity(salt, AccountIdentityInput::AgyLoopLogin(&meta))
                }
                cred => compute_account_identity(
                    salt,
                    AccountIdentityInput::AgyDefaultFile(cred.is_some()),
                ),
            }
        }
    };
    Some(CliIdentity {
        executable,
        account,
        sandbox: sandbox_kind.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_cli_identities(
    providers: &[Provider],
    free_outcomes: &BTreeMap<Provider, ProviderFreeOutcome>,
    home_dir: &Path,
    agy_state_dir: Option<&Path>,
    opencode_api_key: Option<&str>,
    sandbox_kind: &str,
    salt: &[u8; 32],
    path_var: Option<&std::ffi::OsStr>,
) -> BTreeMap<Provider, CliIdentity> {
    let mut map = BTreeMap::new();
    for &provider in providers {
        if let Some(identity) = build_cli_identity_for_provider(
            provider,
            free_outcomes,
            home_dir,
            agy_state_dir,
            opencode_api_key,
            sandbox_kind,
            salt,
            path_var,
        ) {
            map.insert(provider, identity);
        }
    }
    map
}

fn build_free_checks_for_targets(
    targets: &[ProbeTarget],
    free_outcomes: &BTreeMap<Provider, ProviderFreeOutcome>,
    home_dir: &Path,
    agy_state_dir: Option<&Path>,
    opencode_api_key: Option<&str>,
) -> Vec<FreeCheck> {
    targets
        .iter()
        .map(|target| match target.choice.provider {
            Provider::Claude => {
                let outcome = free_outcomes.get(&Provider::Claude);
                match outcome.and_then(|o| o.claude_status.as_ref()) {
                    Some(ClaudeStatus::LoggedIn { .. }) => FreeCheck {
                        account_ok: Ok(()),
                        listed: Listed::Yes,
                    },
                    Some(ClaudeStatus::LoggedOut) => FreeCheck {
                        account_ok: Err(
                            "conta do 'claude' desconectada; rode 'tchelvis loop auth login claude'"
                                .to_string(),
                        ),
                        listed: Listed::No,
                    },
                    _ => {
                        if !non_empty_regular_file(&home_dir.join(".claude/.credentials.json")) {
                            FreeCheck {
                                account_ok: Err(
                                    "conta do 'claude' ausente (~/.claude/.credentials.json); rode 'tchelvis loop auth login claude'"
                                        .to_string(),
                                ),
                                listed: Listed::Unknown,
                            }
                        } else {
                            FreeCheck {
                                account_ok: Ok(()),
                                listed: Listed::Unknown,
                            }
                        }
                    }
                }
            }
            Provider::Agy => {
                let outcome = free_outcomes.get(&Provider::Agy);
                let token_present = resolve_agy_credential(home_dir, agy_state_dir).is_some();
                let account_ok = if !token_present {
                    Err(AGY_ABSENT_REASON.to_string())
                } else if let Some(out) = outcome {
                    out.account_ok.clone()
                } else {
                    Ok(())
                };
                let listed = match outcome {
                    Some(out) if out.list_known => is_model_listed(
                        Provider::Agy,
                        target.choice.model.as_deref(),
                        &out.models,
                    ),
                    _ => Listed::Unknown,
                };
                FreeCheck { account_ok, listed }
            }
            Provider::Opencode => {
                let outcome = free_outcomes.get(&Provider::Opencode);
                let account_ok =
                    if needs_opencode_key(target.choice.model.as_deref()) && opencode_api_key.is_none()
                    {
                        Err(
                            "chave OPENCODE_API_KEY ausente para um modelo 'opencode-go/' (exporte OPENCODE_API_KEY ou use um modelo gratuito 'opencode/...')"
                                .to_string(),
                        )
                    } else if let Some(out) = outcome {
                        out.account_ok.clone()
                    } else {
                        Ok(())
                    };
                let listed = match outcome {
                    Some(out) if out.list_known => is_model_listed(
                        Provider::Opencode,
                        target.choice.model.as_deref(),
                        &out.models,
                    ),
                    _ => Listed::Unknown,
                };
                FreeCheck { account_ok, listed }
            }
            Provider::Pi => {
                let outcome = free_outcomes.get(&Provider::Pi);
                let prefix = target
                    .choice
                    .model
                    .as_deref()
                    .and_then(|m| m.split_once('/'))
                    .map(|(p, _)| p.to_string());
                let account_ok = match (prefix.as_deref(), outcome) {
                    (Some(p), Some(out)) => match out.pi_accounts.get(p) {
                        Some(PiAccount::NotReady(reason)) => Err(format!(
                            "provedor '{p}' do pi sem credencial ({reason}); rode 'tchelvis loop auth login pi {p}'"
                        )),
                        _ => Ok(()),
                    },
                    _ => Ok(()),
                };
                let listed = match outcome {
                    Some(out) if out.list_known => is_model_listed(
                        Provider::Pi,
                        target.choice.model.as_deref(),
                        &out.models,
                    ),
                    _ => Listed::Unknown,
                };
                FreeCheck { account_ok, listed }
            }
        })
        .collect()
}

fn format_plan_failures(planned: &[PlannedCheck], extra_secrets: &[String]) -> Option<String> {
    let fails: Vec<&PlannedCheck> = planned
        .iter()
        .filter(|p| matches!(p.action, PlanAction::Fail(_)))
        .collect();
    if fails.is_empty() {
        return None;
    }
    let refs: Vec<&str> = extra_secrets.iter().map(|s| s.as_str()).collect();
    let mut msg = String::from("pré-verificação falhou (nada foi criado):\n");
    for fail in fails {
        if let PlanAction::Fail(reason) = &fail.action {
            let _ = writeln!(
                msg,
                "  - {} (etapas: {}): {}",
                fail.target.choice.display(),
                fail.target.stage_ids.join(", "),
                reason
            );
        }
    }
    Some(redact_loop_text(msg.trim_end(), &refs))
}

fn format_probe_failures(failures: &[ProbeFailure], extra_secrets: &[String]) -> String {
    let mut msg = String::from("pré-teste de modelos falhou (nada foi criado):\n");
    for fail in failures {
        let _ = writeln!(
            msg,
            "  - {} (etapas: {}): {}",
            fail.choice.display(),
            fail.stage_ids.join(", "),
            fail.reason
        );
    }
    let refs: Vec<&str> = extra_secrets.iter().map(|s| s.as_str()).collect();
    redact_loop_text(msg.trim_end(), &refs)
}

#[allow(clippy::too_many_arguments)]
fn record_probe_evidence(
    evidence: &mut EvidenceStore,
    records: &[ProbeRecord],
    free_outcomes: &BTreeMap<Provider, ProviderFreeOutcome>,
    home_dir: &Path,
    agy_state_dir: Option<&Path>,
    opencode_api_key: Option<&str>,
    sandbox_kind: &str,
    now_ms: i64,
    extra_secrets: &[String],
) {
    let path_var = std::env::var_os("PATH");
    for rec in records {
        let choice = ModelChoice {
            provider: rec.provider,
            model: rec.model.clone(),
        };
        if let Err(err) = evidence.record_with(&choice, now_ms, |salt| {
            build_cli_identity_for_provider(
                rec.provider,
                free_outcomes,
                home_dir,
                agy_state_dir,
                opencode_api_key,
                sandbox_kind,
                salt,
                path_var.as_deref(),
            )
        }) {
            eprint_redacted(
                &format!(
                    "aviso: falha ao gravar evidência de {}: {err:#}",
                    choice.display()
                ),
                extra_secrets,
            );
        }
    }
}

fn discard_failed_probes(
    evidence: &mut EvidenceStore,
    failures: &[ProbeFailure],
    extra_secrets: &[String],
) {
    for fail in failures {
        if let Err(err) = evidence.discard(&fail.choice) {
            eprint_redacted(
                &format!(
                    "aviso: falha ao descartar evidência de {}: {err:#}",
                    fail.choice.display()
                ),
                extra_secrets,
            );
        }
    }
}

fn assemble_preflight_records(
    planned: &[PlannedCheck],
    probed: Vec<ProbeRecord>,
) -> Vec<ProbeRecord> {
    let mut probed_iter = probed.into_iter();
    let mut out = Vec::with_capacity(planned.len());
    for item in planned {
        match &item.action {
            PlanAction::Evidence => out.push(ProbeRecord {
                provider: item.target.choice.provider,
                model: item.target.choice.model.clone(),
                method: ProbeMethod::Evidence,
                usage: Usage::default(),
                duration_ms: 0,
            }),
            PlanAction::FreeOnly => out.push(ProbeRecord {
                provider: item.target.choice.provider,
                model: item.target.choice.model.clone(),
                method: ProbeMethod::FreeOnly,
                usage: Usage::default(),
                duration_ms: 0,
            }),
            PlanAction::Probe => {
                if let Some(rec) = probed_iter.next() {
                    out.push(rec);
                }
            }
            PlanAction::Fail(_) => {}
        }
    }
    out
}

struct StoreEvidenceRecorder {
    path: PathBuf,
    salt: Option<[u8; 32]>,
    identities: BTreeMap<Provider, CliIdentity>,
}

impl EvidenceRecorder for StoreEvidenceRecorder {
    fn record(&mut self, provider: Provider, model: Option<&str>, now_ms: i64) {
        let Some(identity) = self.identities.get(&provider).cloned() else {
            return;
        };
        let mut store = EvidenceStore::open(&self.path);
        if let Some(salt) = self.salt {
            store.set_salt_if_uninitialized(salt);
        }
        let choice = ModelChoice {
            provider,
            model: model.map(ToString::to_string),
        };
        if self.salt.is_none_or(|s| *store.salt() == s) {
            let _ = store.record(&choice, &identity, now_ms);
        }
    }

    fn discard(&mut self, provider: Provider, model: Option<&str>) {
        let mut store = EvidenceStore::open(&self.path);
        let choice = ModelChoice {
            provider,
            model: model.map(ToString::to_string),
        };
        let _ = store.discard(&choice);
    }
}

fn collect_extra_secrets(opencode_api_key: Option<&str>) -> Vec<String> {
    let mut secrets = Vec::new();
    if let Some(k) = opencode_api_key {
        if !k.is_empty() {
            secrets.push(k.to_string());
        }
    }
    if let Ok(env_k) = std::env::var("OPENCODE_API_KEY") {
        let trimmed = env_k.trim();
        if !trimmed.is_empty() && !secrets.iter().any(|s| s == trimmed) {
            secrets.push(trimmed.to_string());
        }
    }
    secrets
}

fn for_terminal(text: &str, extra_secrets: &[String]) -> String {
    let refs: Vec<&str> = extra_secrets.iter().map(|s| s.as_str()).collect();
    crate::loops::report::sanitize_multiline(text, &refs)
}

fn eprint_redacted(msg: &str, extra_secrets: &[String]) {
    eprintln!("{}", for_terminal(msg, extra_secrets));
}

pub fn build_generated_workflow_and_roles(
    name: &str,
    stage_roles: &[String],
) -> Result<(String, BTreeMap<String, String>)> {
    if !is_valid_slug(name, 48) {
        bail!("nome de workflow inválido '{name}' (use apenas [a-z0-9-]+)");
    }
    if stage_roles.is_empty() {
        bail!("--stages requer pelo menos uma etapa");
    }

    struct GenStage {
        id: String,
        role: String,
        edits: bool,
        writes: Option<String>,
        reads: Vec<String>,
        next: Option<String>,
        verdict_gate: bool,
        on_pass: Option<String>,
        on_retry: Option<String>,
    }

    let mut role_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut stem_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut stages: Vec<GenStage> = Vec::with_capacity(stage_roles.len());

    for raw_role in stage_roles {
        let role = raw_role.trim();
        if !is_valid_role_name(role) {
            bail!("nome de role inválido '{role}' (use apenas [a-z0-9-]+)");
        }
        let count = {
            let entry = role_counts.entry(role.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        let id = if count == 1 {
            role.to_string()
        } else {
            format!("{role}-{count}")
        };
        if !is_valid_stage_id(&id) {
            bail!("id de etapa gerado '{id}' é inválido ou excede 32 caracteres");
        }

        let edits = matches!(role, "worker" | "tester");
        let writes = if role == "worker" {
            None
        } else {
            let stem = match role {
                "designer" | "planner" => "design",
                "reviewer" => "review",
                "tester" => "test-report",
                other => other,
            };
            let s_count = {
                let entry = stem_counts.entry(stem.to_string()).or_insert(0);
                *entry += 1;
                *entry
            };
            Some(if s_count == 1 {
                format!("{stem}.md")
            } else {
                format!("{stem}-{s_count}.md")
            })
        };

        stages.push(GenStage {
            id,
            role: role.to_string(),
            edits,
            writes,
            reads: Vec::new(),
            next: None,
            verdict_gate: false,
            on_pass: None,
            on_retry: None,
        });
    }

    let Some(first_edit_idx) = stages.iter().position(|s| s.edits) else {
        bail!("o workflow precisa de pelo menos uma etapa que edita (worker ou tester)");
    };
    if stages.last().is_some_and(|s| s.edits) {
        bail!("a última etapa do workflow não pode ter edits = true (precisa ser um portão de veredito)");
    }

    let total = stages.len();
    for i in 0..total {
        let mut reads = vec!["input.md".to_string()];
        let mut saw_editor = false;
        for (j, prev) in stages.iter().enumerate().take(i) {
            if let Some(w) = &prev.writes {
                if !reads.contains(w) {
                    reads.push(w.clone());
                }
            }
            if prev.edits && !saw_editor {
                saw_editor = true;
                // Place `current.diff` right where edits become visible.
                if j < i && !reads.iter().any(|r| r == "current.diff") {
                    reads.push("current.diff".to_string());
                }
            }
        }
        if i == first_edit_idx {
            for later in stages.iter().skip(i + 1) {
                if let Some(w) = &later.writes {
                    if !reads.contains(w) {
                        reads.push(w.clone());
                    }
                }
            }
        }
        stages[i].reads = reads;

        if i + 1 < total {
            stages[i].next = Some(stages[i + 1].id.clone());
        } else {
            stages[i].verdict_gate = true;
            stages[i].on_pass = Some("done".to_string());
            stages[i].on_retry = Some(stages[first_edit_idx].id.clone());
        }
    }

    let mut toml = String::new();
    let _ = writeln!(toml, "name = {:?}", name);
    let _ = writeln!(
        toml,
        "description = {:?}",
        format!("Workflow gerado: {}", stage_roles.join(" -> "))
    );
    let _ = writeln!(toml, "max_cycles = {DEFAULT_MAX_CYCLES}");
    let _ = writeln!(toml, "stage_timeout_secs = {DEFAULT_STAGE_TIMEOUT_SECS}");
    let _ = writeln!(toml, "stall_secs = {DEFAULT_STALL_SECS}");
    let _ = writeln!(toml, "max_duration_secs = {DEFAULT_MAX_DURATION_SECS}");

    let mut new_roles = BTreeMap::new();
    for st in &stages {
        let _ = writeln!(toml, "\n[[stages]]");
        let _ = writeln!(toml, "id = {:?}", st.id);
        let _ = writeln!(toml, "role = {:?}", st.role);
        let _ = writeln!(toml, "reads = {:?}", st.reads);
        if let Some(w) = &st.writes {
            let _ = writeln!(toml, "writes = {:?}", w);
        }
        if st.edits {
            let _ = writeln!(toml, "edits = true");
        }
        if let Some(next) = &st.next {
            let _ = writeln!(toml, "next = {:?}", next);
        }
        if st.verdict_gate {
            let _ = writeln!(toml, "verdict_gate = true");
        }
        if let Some(on_pass) = &st.on_pass {
            let _ = writeln!(toml, "on_pass = {:?}", on_pass);
        }
        if let Some(on_retry) = &st.on_retry {
            let _ = writeln!(toml, "on_retry = {:?}", on_retry);
        }

        let is_builtin = DEFAULT_ROLES.iter().any(|(r, _)| *r == st.role);
        if !is_builtin && !new_roles.contains_key(&st.role) {
            let out_file = st.writes.as_deref().unwrap_or("output.md");
            let mut body = format!(
                "Você atua no papel '{}'.\n\n\
                 Leia os artefatos fornecidos em `.tchelvis-loop/in/`, execute apenas análise em modo somente leitura sem modificar arquivos do repositório e registre sua saída em `.tchelvis-loop/out/{out_file}`.\n",
                st.role
            );
            if st.verdict_gate {
                body.push_str(
                    "\nTermine `.tchelvis-loop/out/` com uma última linha exata: `VERDICT: PASS` ou `VERDICT: RETRY`.\n",
                );
            }
            new_roles.insert(st.role.clone(), body);
        }
    }

    parse_workflow_unresolved(&toml)
        .map_err(|errs| anyhow::anyhow!("TOML gerado inválido:\n- {}", errs.join("\n- ")))?;

    Ok((toml, new_roles))
}

fn default_loops_toml_template() -> &'static str {
    "# Configuração global de tchelvis loop\n\
     max_concurrent = 2\n\
     # Defina true após revisar que os CLIs rodam com aprovação automática apenas dentro do sandbox.\n\
     auto_approve_in_sandbox = false\n\
     env_pass = []\n\
     \n\
     [sandbox]\n\
     kind = \"bwrap\"\n\
     \n\
     [sandbox.bwrap]\n\
     home_ro = [\".cargo\", \".rustup\"]\n\
     \n\
     # [preflight]\n\
     # mode = \"auto\"      # \"auto\" | \"always\" | \"free\"\n\
     # valid_hours = 12   # 1..=168\n\
     \n\
     # [defaults.worker]\n\
     # provider = \"claude\"\n\
     # model = \"sonnet\"\n"
}

fn cmd_init(paths: &CockpitPaths) -> Result<(), LoopError> {
    fs::create_dir_all(&paths.config_dir).map_err(|e| {
        LoopError::err(format!(
            "falha ao criar {}: {e:#}",
            paths.config_dir.display()
        ))
    })?;
    let global_cfg = paths.config_dir.join("loops.toml");
    let created_cfg = write_if_absent_nofollow(&global_cfg, default_loops_toml_template(), 0o600)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;

    if created_cfg {
        println!(
            "Criado: {} (defina auto_approve_in_sandbox = true após revisar)",
            global_cfg.display()
        );
    } else {
        println!("Mantido: {}", global_cfg.display());
    }

    if let Some(repo) = try_find_git_repo_root() {
        let _ = ensure_dir_no_symlink(&repo, &[".loops"])
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
        let wf_dir = ensure_dir_no_symlink(&repo, &[".loops", "workflows"])
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
        let roles_dir = ensure_dir_no_symlink(&repo, &[".loops", "roles"])
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
        let runs_dir = ensure_dir_no_symlink(&repo, &[".loops", "runs"])
            .map_err(|e| LoopError::err(format!("{e:#}")))?;

        let gitignore = runs_dir.join(".gitignore");
        let _ = write_if_absent_nofollow(&gitignore, "*\n", 0o644)
            .map_err(|e| LoopError::err(format!("{e:#}")))?;

        for (name, content) in PRESET_WORKFLOWS {
            let p = wf_dir.join(format!("{name}.toml"));
            let _ = write_if_absent_nofollow(&p, content, 0o644)
                .map_err(|e| LoopError::err(format!("{e:#}")))?;
        }
        for (name, content) in DEFAULT_ROLES {
            let p = roles_dir.join(format!("{name}.md"));
            let _ = write_if_absent_nofollow(&p, content, 0o644)
                .map_err(|e| LoopError::err(format!("{e:#}")))?;
        }
        println!(
            "Inicializado repositório: {}",
            repo.join(".loops").display()
        );
    } else {
        println!("Fora de um repositório git: apenas a configuração global foi verificada.");
    }

    Ok(())
}

fn cmd_new(paths: &CockpitPaths, name: &str, stages: &[String]) -> Result<(), LoopError> {
    let repo = find_git_repo_root().map_err(|e| LoopError::usage(format!("{e:#}")))?;
    if !is_valid_slug(name, 48) {
        return Err(LoopError::usage(format!(
            "nome de workflow inválido '{name}' (use apenas [a-z0-9-]+)"
        )));
    }

    if let Some(dir) = open_dir_nofollow_from_root(&repo, &[".loops", "workflows"])
        .map_err(|e| LoopError::usage(format!("{e:#}")))?
    {
        if read_regular_file_at(
            dir.as_fd(),
            &format!("{name}.toml"),
            MAX_WORKFLOW_TOML_BYTES,
        )
        .map_err(|e| LoopError::usage(format!("{e:#}")))?
        .is_some()
        {
            return Err(LoopError::usage(format!(
                "workflow '{name}' já existe em {}",
                repo.join(".loops/workflows")
                    .join(format!("{name}.toml"))
                    .display()
            )));
        }
    }
    let target_wf_path = repo.join(".loops/workflows").join(format!("{name}.toml"));
    if fs::symlink_metadata(&target_wf_path).is_ok() {
        return Err(LoopError::usage(format!(
            "arquivo '{}' já existe",
            target_wf_path.display()
        )));
    }

    let (toml_str, new_roles) = build_generated_workflow_and_roles(name, stages)
        .map_err(|e| LoopError::usage(format!("{e:#}")))?;

    let _ =
        ensure_dir_no_symlink(&repo, &[".loops"]).map_err(|e| LoopError::err(format!("{e:#}")))?;
    let wf_dir = ensure_dir_no_symlink(&repo, &[".loops", "workflows"])
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
    let roles_dir = ensure_dir_no_symlink(&repo, &[".loops", "roles"])
        .map_err(|e| LoopError::err(format!("{e:#}")))?;

    for (role_name, role_md) in new_roles {
        let global_exists = open_dir_nofollow_from_root(&paths.config_dir, &["loops", "roles"])
            .ok()
            .flatten()
            .and_then(|d| {
                read_regular_file_at(d.as_fd(), &format!("{role_name}.md"), MAX_ROLE_MD_BYTES).ok()
            })
            .flatten()
            .is_some();
        if !global_exists {
            let role_path = roles_dir.join(format!("{role_name}.md"));
            let _ = write_if_absent_nofollow(&role_path, &role_md, 0o644)
                .map_err(|e| LoopError::err(format!("{e:#}")))?;
        }
    }

    let wf_path = wf_dir.join(format!("{name}.toml"));
    if !write_if_absent_nofollow(&wf_path, &toml_str, 0o644)
        .map_err(|e| LoopError::err(format!("{e:#}")))?
    {
        return Err(LoopError::usage(format!(
            "workflow '{name}' já existe em {}",
            wf_path.display()
        )));
    }

    print!("{toml_str}");
    Ok(())
}

fn collect_workflow_catalog(
    repo_root: Option<&Path>,
    paths: &CockpitPaths,
) -> Result<BTreeMap<String, (WorkflowOrigin, UnresolvedWorkflow)>> {
    let mut catalog = BTreeMap::new();
    for (name, raw) in PRESET_WORKFLOWS {
        if let Ok(wf) = parse_workflow_unresolved(raw) {
            catalog.insert((*name).to_string(), (WorkflowOrigin::Preset, wf));
        }
    }
    if let Some(dir) = open_dir_nofollow_from_root(&paths.config_dir, &["loops", "workflows"])? {
        let global_path = paths.config_dir.join("loops/workflows");
        if let Ok(entries) = fs::read_dir(&global_path) {
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if let Some(slug) = fname.strip_suffix(".toml") {
                    if is_valid_slug(slug, 48) {
                        if let Ok(Some(raw)) =
                            read_regular_file_at(dir.as_fd(), &fname, MAX_WORKFLOW_TOML_BYTES)
                        {
                            if let Ok(wf) = parse_workflow_unresolved(&raw) {
                                catalog.insert(slug.to_string(), (WorkflowOrigin::Global, wf));
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(repo) = repo_root {
        if let Some(dir) = open_dir_nofollow_from_root(repo, &[".loops", "workflows"])? {
            let repo_wf_path = repo.join(".loops/workflows");
            if let Ok(entries) = fs::read_dir(&repo_wf_path) {
                for entry in entries.flatten() {
                    let fname = entry.file_name().to_string_lossy().into_owned();
                    if let Some(slug) = fname.strip_suffix(".toml") {
                        if is_valid_slug(slug, 48) {
                            if let Ok(Some(raw)) =
                                read_regular_file_at(dir.as_fd(), &fname, MAX_WORKFLOW_TOML_BYTES)
                            {
                                if let Ok(wf) = parse_workflow_unresolved(&raw) {
                                    catalog.insert(slug.to_string(), (WorkflowOrigin::Repo, wf));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(catalog)
}

fn cmd_list(paths: &CockpitPaths, runs: bool, active: bool) -> Result<(), LoopError> {
    if runs || active {
        let empty_msg = if active {
            "Nenhum loop em execução."
        } else {
            "Nenhuma execução registrada."
        };
        let db_path = paths.orch_db_file();
        if !db_path.exists() {
            println!("{empty_msg}");
            return Ok(());
        }
        let store = OrchStore::open(&db_path).map_err(|e| LoopError::err(format!("{e:#}")))?;
        let mut items = store
            .list_loops()
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
        if active {
            items.retain(is_active);
        }
        if items.is_empty() {
            println!("{empty_msg}");
            return Ok(());
        }
        for rec in items {
            let notify_note = if rec.notify_state == "undelivered" {
                " [aviso não entregue]"
            } else if rec.notify_state == "pending" {
                " [aviso pendente]"
            } else {
                ""
            };
            println!(
                "{}\t{}\t{}{}",
                rec.run_id, rec.workflow, rec.status, notify_note
            );
        }
        return Ok(());
    }

    let repo = try_find_git_repo_root();
    let catalog = collect_workflow_catalog(repo.as_deref(), paths)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
    if catalog.is_empty() {
        println!("Nenhum workflow disponível.");
        return Ok(());
    }
    for (name, (origin, wf)) in catalog {
        let origin_label = match origin {
            WorkflowOrigin::Repo => "projeto",
            WorkflowOrigin::Global => "global",
            WorkflowOrigin::Preset => "preset",
        };
        let desc = if wf.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", sanitize_single_line(&wf.description, Some(200)))
        };
        println!("{name}\t[{origin_label}]{desc}");
    }
    Ok(())
}

fn parse_use_overrides(raw_flags: &[String]) -> Result<Vec<(String, ModelChoice)>, LoopError> {
    let mut parsed = Vec::with_capacity(raw_flags.len());
    for raw in raw_flags {
        let item = parse_use_flag(raw).map_err(|e| LoopError::usage(format!("{e:#}")))?;
        parsed.push(item);
    }
    Ok(parsed)
}

fn cmd_models(
    paths: &CockpitPaths,
    action: Option<ModelsSubcommand>,
    workflow: Option<String>,
) -> Result<(), LoopError> {
    match action {
        Some(ModelsSubcommand::Set {
            spec,
            project,
            global,
        }) => {
            if !project && !global {
                return Err(LoopError::usage(
                    "informe --project ou --global em 'tchelvis loop models set'",
                ));
            }
            let (role, choice) =
                parse_use_flag(&spec).map_err(|e| LoopError::usage(format!("{e:#}")))?;
            if !is_valid_role_name(&role) {
                return Err(LoopError::usage(format!(
                    "nome de role inválido '{role}' (use apenas [a-z0-9-]+)"
                )));
            }
            if project {
                let repo = find_git_repo_root().map_err(|e| LoopError::usage(format!("{e:#}")))?;
                let loops_dir = ensure_dir_no_symlink(&repo, &[".loops"])
                    .map_err(|e| LoopError::err(format!("{e:#}")))?;
                let models_path = loops_dir.join("models.toml");
                write_default_in_toml_file(&models_path, &role, &choice, true)
                    .map_err(|e| LoopError::err(format!("{e:#}")))?;
                println!(
                    "Atualizado [defaults.{role}] = {} em {}",
                    choice.display(),
                    models_path.display()
                );
            } else {
                fs::create_dir_all(&paths.config_dir).map_err(|e| {
                    LoopError::err(format!(
                        "falha ao criar {}: {e:#}",
                        paths.config_dir.display()
                    ))
                })?;
                let cfg_path = paths.config_dir.join("loops.toml");
                write_default_in_toml_file(&cfg_path, &role, &choice, false)
                    .map_err(|e| LoopError::err(format!("{e:#}")))?;
                println!(
                    "Atualizado [defaults.{role}] = {} em {}",
                    choice.display(),
                    cfg_path.display()
                );
            }
            Ok(())
        }
        Some(ModelsSubcommand::Confirm { workflow }) => {
            let repo = find_git_repo_root().map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let cfg = load_loops_config_optional(paths)
                .map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let defaults: RoleDefaults =
                cfg.as_ref().map(|c| c.defaults.clone()).unwrap_or_default();
            let (unresolved, origin) =
                load_workflow_unresolved(&workflow, Some(&repo), paths, None)
                    .map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let project_models = load_project_models_from_repo(&repo)
                .map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let resolution =
                resolve_workflow(unresolved, origin, &[], project_models.as_ref(), &defaults)
                    .map_err(|errs| LoopError::usage(errs.join("\n")))?;

            if !resolution.needs_confirmation() {
                println!(
                    "Workflow '{workflow}' não possui escolhas vindas do repositório; confirmação desnecessária."
                );
                return Ok(());
            }

            let confirm_path = paths.state_dir.join("loops/confirmed.json");
            record_confirmation(&confirm_path, &repo, &resolution)
                .map_err(|e| LoopError::err(format!("{e:#}")))?;
            println!("Confirmação registrada para '{workflow}':");
            for st in &resolution.stages {
                if st.from_repo {
                    println!(
                        "  {} ({}): {} [{}]",
                        st.stage_id,
                        st.role,
                        st.choice.display(),
                        st.source
                    );
                }
            }
            Ok(())
        }
        None => {
            let Some(wf_name) = workflow else {
                return Err(LoopError::usage(
                    "uso: tchelvis loop models <workflow> | models set <role>=<cli[:modelo]> (--project|--global) | models confirm <workflow>",
                ));
            };
            let repo = try_find_git_repo_root();
            let cfg = load_loops_config_optional(paths)
                .map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let defaults: RoleDefaults =
                cfg.as_ref().map(|c| c.defaults.clone()).unwrap_or_default();
            let (unresolved, origin) =
                load_workflow_unresolved(&wf_name, repo.as_deref(), paths, None)
                    .map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let project_models = match &repo {
                Some(r) => load_project_models_from_repo(r)
                    .map_err(|e| LoopError::usage(format!("{e:#}")))?,
                None => None,
            };
            let resolution =
                resolve_workflow(unresolved, origin, &[], project_models.as_ref(), &defaults)
                    .map_err(|errs| LoopError::usage(errs.join("\n")))?;

            println!("Modelos efetivos de '{wf_name}':");
            for st in &resolution.stages {
                let repo_mark = if st.from_repo { " [repositório]" } else { "" };
                println!(
                    "  {} ({}): {} — origem: {}{}",
                    st.stage_id,
                    st.role,
                    st.choice.display(),
                    st.source,
                    repo_mark
                );
            }
            if resolution.needs_confirmation() {
                if let Some(r) = &repo {
                    let confirm_path = paths.state_dir.join("loops/confirmed.json");
                    let confirmed = is_confirmed(&confirm_path, r, &resolution).unwrap_or(false);
                    if !confirmed {
                        println!(
                            "Confirmação pendente para este repositório: rode 'tchelvis loop models confirm {wf_name}'."
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

async fn cmd_auth(paths: &CockpitPaths, action: Option<AuthSubcommand>) -> Result<(), LoopError> {
    match action {
        None => {
            let opencode_key = resolve_opencode_api_key();
            let extra = collect_extra_secrets(opencode_key.as_deref());
            let cfg = match load_loops_config_optional(paths) {
                Ok(Some(c)) => c,
                Ok(None) => LoopsConfig {
                    max_concurrent: DEFAULT_MAX_CONCURRENT,
                    auto_approve_in_sandbox: false,
                    env_pass: Vec::new(),
                    sandbox: SandboxConfig::Bwrap {
                        home_ro: Vec::new(),
                    },
                    defaults: RoleDefaults::default(),
                    preflight: PreflightConfig::default(),
                },
                Err(e) => return Err(LoopError::usage(format!("{e:#}"))),
            };
            ensure_sandbox_prereqs(&cfg)?;
            let sandbox = build_sandbox(&cfg, paths);
            let auth_dir = TempDirGuard::create(paths.state_dir.join("loops").join(format!(
                ".auth-{}-{}",
                std::process::id(),
                generate_run_id()
            )))
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
            let cancel = install_cancel_signal_flag();
            let clock = SystemClock::new();
            let providers = [
                Provider::Claude,
                Provider::Agy,
                Provider::Opencode,
                Provider::Pi,
            ];
            let outcomes = run_free_checks_with_options(
                &providers,
                sandbox.as_ref(),
                auth_dir.path(),
                &paths.home_dir,
                &cancel,
                &clock,
                opencode_key.as_deref(),
                &[],
            )
            .map_err(LoopError::cancelled)?;
            drop(auth_dir);

            let claude_cred =
                non_empty_regular_file(&paths.home_dir.join(".claude/.credentials.json"));
            let agy_cred =
                resolve_agy_credential(&paths.home_dir, sandbox.agy_state_dir()).is_some();

            let claude_line = match outcomes
                .get(&Provider::Claude)
                .and_then(|o| o.claude_status.as_ref())
            {
                Some(ClaudeStatus::LoggedIn { .. }) => {
                    "claude:   autenticado (claude auth status)".to_string()
                }
                Some(ClaudeStatus::LoggedOut) => {
                    "claude:   ausente (rode 'tchelvis loop auth login claude')".to_string()
                }
                Some(ClaudeStatus::Unknown(reason)) => {
                    if !claude_cred {
                        "claude:   ausente (arquivo ~/.claude/.credentials.json; rode 'tchelvis loop auth login claude')"
                            .to_string()
                    } else {
                        format!("claude:   indeterminado ({})", for_terminal(reason, &extra))
                    }
                }
                None => {
                    if claude_cred {
                        "claude:   autenticado (arquivo ~/.claude/.credentials.json)".to_string()
                    } else {
                        "claude:   ausente (arquivo ~/.claude/.credentials.json)".to_string()
                    }
                }
            };

            let agy_line = match outcomes.get(&Provider::Agy) {
                Some(out) if !agy_cred || out.account_ok.is_err() => {
                    format!("agy:      ausente ({AGY_ABSENT_REASON})")
                }
                Some(out) if out.list_known => {
                    let count = out.models.len() / 2;
                    format!("agy:      autenticado ({count} modelo(s) listado(s) por 'agy models')")
                }
                _ => {
                    if agy_cred {
                        "agy:      autenticado (token presente; lista indisponível em 'agy models')"
                            .to_string()
                    } else {
                        format!("agy:      ausente ({AGY_ABSENT_REASON})")
                    }
                }
            };

            let key_label = if opencode_key.is_some() {
                "chave presente"
            } else {
                "sem chave"
            };
            let opencode_line = match outcomes.get(&Provider::Opencode) {
                Some(out) if out.list_known => format!(
                    "opencode: {key_label} ({} modelo(s) listado(s) por 'opencode models'; modelos gratuitos 'opencode/...' funcionam sem chave)",
                    out.models.len()
                ),
                _ => format!(
                    "opencode: {key_label} (OPENCODE_API_KEY; modelos gratuitos 'opencode/...' funcionam sem chave)"
                ),
            };

            let pi_install =
                pi_managed_install(&paths.home_dir, std::env::var_os("PATH").as_deref());
            let pi_line = pi_auth_line(outcomes.get(&Provider::Pi), &pi_install);

            println!("{claude_line}");
            println!("{agy_line}");
            println!("{opencode_line}");
            println!("{pi_line}");
            Ok(())
        }
        Some(AuthSubcommand::Login { cli, provider }) => {
            let herdr = HerdrClient::default();
            let caller_id = std::env::var("HC_TERMINAL_ID").unwrap_or_default();
            if caller_id.trim().is_empty() {
                return Err(LoopError::err(
                    "HC_TERMINAL_ID ausente: execute 'tchelvis loop auth login' a partir de um terminal Herdr",
                ));
            }
            let argv: Vec<String> = match cli.as_str() {
                // Redirect ~/.gemini/antigravity-cli to <state>/loops/agy-login with file-only
                // token storage while keeping D-Bus/XDG_RUNTIME_DIR intact so the browser opens.
                "agy" => {
                    // The absolute path: a Herdr terminal has its own `PATH`.
                    let bwrap = find_in_path("bwrap").ok_or_else(|| {
                        LoopError::err(
                            "bubblewrap ('bwrap') não encontrado no PATH; instale o pacote 'bubblewrap' (ex.: 'sudo apt install bubblewrap' ou 'sudo pacman -S bubblewrap') para separar o login do 'agy'",
                        )
                    })?;
                    let login_dir = ensure_private_agy_login_dir(&paths.state_dir)
                        .map_err(|e| LoopError::err(format!("{e:#}")))?;
                    let target_dir = ensure_agy_target_dir(&paths.home_dir)
                        .map_err(|e| LoopError::err(format!("{e:#}")))?;
                    vec![
                        bwrap.to_string_lossy().into_owned(),
                        "--dev-bind".into(),
                        "/".into(),
                        "/".into(),
                        "--bind".into(),
                        login_dir.to_string_lossy().into_owned(),
                        target_dir.to_string_lossy().into_owned(),
                        "--die-with-parent".into(),
                        "--".into(),
                        "/usr/bin/env".into(),
                        "AGY_CLI_FORCE_FILE_TOKEN_STORAGE=1".into(),
                        "agy".into(),
                    ]
                }
                _ => login_argv(&cli, provider.as_deref())?,
            };
            let cwd = std::env::current_dir().unwrap_or_else(|_| paths.home_dir.clone());
            let term = herdr
                .run_worker(&caller_id, &cwd, &[], &argv, "")
                .await
                .map_err(|e| {
                    LoopError::err(format!("falha ao abrir terminal de login via Herdr: {e:#}"))
                })?;
            println!("Terminal de login aberto para '{cli}': {}", term.address);
            if cli == "agy" {
                println!(
                    "Esse login é só do loop (salvo em arquivo, sem chaveiro); se o navegador não abrir, copie a URL exibida no terminal."
                );
            }
            println!("Para abrir o terminal: hc attach {}", term.address);
            Ok(())
        }
    }
}

fn open_private_dir_nofollow(dir: &Path, ensure_mode_0700: bool) -> Result<OwnedFd> {
    let raw = dir.to_str().context("caminho não UTF-8")?;
    let c_path = CString::new(raw)?;
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ELOOP) {
            bail!("'{}' é um link simbólico e foi recusado", dir.display());
        }
        return Err(err).with_context(|| format!("falha ao abrir '{}'", dir.display()));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao inspecionar '{}'", dir.display()));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        bail!("'{}' não é um diretório", dir.display());
    }
    let my_uid = unsafe { libc::geteuid() };
    if st.st_uid != my_uid {
        bail!(
            "'{}' pertence a outro usuário (uid {}; esperado {my_uid})",
            dir.display(),
            st.st_uid
        );
    }
    if ensure_mode_0700 && unsafe { libc::fchmod(owned.as_raw_fd(), 0o700) } < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao ajustar permissões 0700 de '{}'", dir.display()));
    }
    Ok(owned)
}

fn ensure_private_subdir_nofollow(parent: &Path, name: &str, force_0700: bool) -> Result<PathBuf> {
    let dir = parent.join(name);
    let mut created = false;
    match fs::symlink_metadata(&dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("'{}' é um link simbólico e foi recusado", dir.display());
            }
            if !meta.file_type().is_dir() {
                bail!("'{}' já existe e não é um diretório", dir.display());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder
                .create(&dir)
                .with_context(|| format!("falha ao criar diretório {}", dir.display()))?;
            created = true;
        }
        Err(err) => {
            return Err(err).with_context(|| format!("falha ao verificar {}", dir.display()));
        }
    }
    let _owned = open_private_dir_nofollow(&dir, force_0700 || created)?;
    Ok(dir)
}

fn ensure_private_agy_login_dir(state_dir: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(state_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!(
                    "'{}' é um link simbólico e foi recusado",
                    state_dir.display()
                );
            }
            if !meta.file_type().is_dir() {
                bail!("'{}' já existe e não é um diretório", state_dir.display());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(state_dir)
                .with_context(|| format!("falha ao criar diretório {}", state_dir.display()))?;
        }
        Err(err) => {
            return Err(err).with_context(|| format!("falha ao verificar {}", state_dir.display()));
        }
    }
    let loops_dir = ensure_dir_no_symlink(state_dir, &["loops"])?;
    let login_dir = ensure_private_subdir_nofollow(&loops_dir, "agy-login", true)?;
    let token_path = login_dir.join(AGY_TOKEN_FILE);
    if let Ok(meta) = fs::symlink_metadata(&token_path) {
        if meta.file_type().is_symlink() {
            bail!(
                "'{}' é um link simbólico e foi recusado",
                token_path.display()
            );
        }
        if !meta.file_type().is_file() {
            bail!(
                "'{}' já existe e não é um arquivo regular",
                token_path.display()
            );
        }
    }
    Ok(login_dir)
}

fn ensure_agy_target_dir(home_dir: &Path) -> Result<PathBuf> {
    let gemini_dir = ensure_private_subdir_nofollow(home_dir, ".gemini", false)?;
    ensure_private_subdir_nofollow(&gemini_dir, "antigravity-cli", false)
}

fn check_project_models_warnings(
    repo: &Path,
    paths: &CockpitPaths,
    project: Option<&ProjectModels>,
) -> Vec<String> {
    let Some(pm) = project else {
        return Vec::new();
    };
    let mut known_workflows: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(catalog) = collect_workflow_catalog(Some(repo), paths) {
        for (name, (_, wf)) in catalog {
            let ids = wf.stages.into_iter().map(|s| s.id).collect();
            known_workflows.insert(name, ids);
        }
    }
    check_project_models_references(pm, &known_workflows)
}

fn install_cancel_signal_flag() -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_task = cancel.clone();
    tokio::spawn(async move {
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = async {
                if let Some(ref mut s) = sigint {
                    s.recv().await
                } else {
                    std::future::pending::<Option<()>>().await
                }
            } => {}
            _ = async {
                if let Some(ref mut s) = sigterm {
                    s.recv().await
                } else {
                    std::future::pending::<Option<()>>().await
                }
            } => {}
        }
        cancel_task.store(true, Ordering::Relaxed);
    });
    cancel
}

fn cmd_check(
    paths: &CockpitPaths,
    workflow_name: &str,
    live: bool,
    use_overrides_raw: &[String],
) -> Result<(), LoopError> {
    let repo = try_find_git_repo_root();

    let cfg = load_loops_config_required(paths).map_err(|e| LoopError::usage(format!("{e:#}")))?;
    let use_overrides = parse_use_overrides(use_overrides_raw)?;

    let (unresolved, origin) =
        load_workflow_unresolved(workflow_name, repo.as_deref(), paths, None)
            .map_err(|e| LoopError::usage(format!("{e:#}")))?;

    let project_models = match &repo {
        Some(r) => {
            load_project_models_from_repo(r).map_err(|e| LoopError::usage(format!("{e:#}")))?
        }
        None => None,
    };
    let resolution = resolve_workflow(
        unresolved,
        origin,
        &use_overrides,
        project_models.as_ref(),
        &cfg.defaults,
    )
    .map_err(|errs| LoopError::usage(errs.join("\n")))?;
    let opencode_key = opencode_key_for(&resolution.workflow);
    let extra = collect_extra_secrets(opencode_key.as_deref());
    let pi_install = pi_managed_install(&paths.home_dir, std::env::var_os("PATH").as_deref());
    for note in pi_check_notes(&resolution.workflow, &pi_install).map_err(LoopError::err)? {
        eprint_redacted(&note, &extra);
    }

    let ev_path = evidence_store_path(paths);
    if let Some(r) = &repo {
        let _roles = load_roles_for_workflow(&resolution.workflow, Some(r), paths)
            .map_err(|e| LoopError::usage(format!("{e:#}")))?;
        let dummy_run = r.join(".loops/runs");
        let dummy_ws = paths.state_dir.join("loops");
        if let SandboxConfig::Bwrap { ref home_ro } = cfg.sandbox {
            check_home_ro_does_not_reach(
                &paths.home_dir,
                home_ro,
                &[&dummy_run, &dummy_ws, &ev_path],
            )
            .map_err(|e| LoopError::usage(format!("{e:#}")))?;
        }
        for warn in check_project_models_warnings(r, paths, project_models.as_ref()) {
            eprintln!("aviso: {warn}");
        }
    } else {
        let _roles = load_roles_for_workflow(&resolution.workflow, None, paths)
            .map_err(|e| LoopError::usage(format!("{e:#}")))?;
        let dummy_ws = paths.state_dir.join("loops");
        if let SandboxConfig::Bwrap { ref home_ro } = cfg.sandbox {
            check_home_ro_does_not_reach(&paths.home_dir, home_ro, &[&dummy_ws, &ev_path])
                .map_err(|e| LoopError::usage(format!("{e:#}")))?;
        }
    }

    ensure_sandbox_prereqs(&cfg)?;
    let sandbox = build_sandbox(&cfg, paths);
    let check_dir = TempDirGuard::create(paths.state_dir.join("loops").join(format!(
        ".check-{}-{}",
        std::process::id(),
        generate_run_id()
    )))
    .map_err(|e| LoopError::err(format!("{e:#}")))?;
    let exposures = sandbox.check(check_dir.path()).map_err(|e| {
        LoopError::err(format!(
            "falha na verificação do sandbox ({}): {e:#}",
            sandbox.kind()
        ))
    })?;

    let targets = probe_targets(&resolution.workflow);
    let providers = distinct_providers(&targets);
    let pi_providers = pi_providers_of(&targets);
    let cancel = install_cancel_signal_flag();
    let clock = SystemClock::new();
    let free_outcomes = run_free_checks_with_options(
        &providers,
        sandbox.as_ref(),
        check_dir.path(),
        &paths.home_dir,
        &cancel,
        &clock,
        opencode_key.as_deref(),
        &pi_providers,
    )
    .map_err(LoopError::cancelled)?;

    let mut evidence = EvidenceStore::open(&ev_path);
    if let Some(warn) = evidence.warning() {
        eprint_redacted(&format!("aviso: {warn}"), &extra);
    }

    let path_var = std::env::var_os("PATH");
    let identities = build_cli_identities(
        &providers,
        &free_outcomes,
        &paths.home_dir,
        sandbox.agy_state_dir(),
        opencode_key.as_deref(),
        sandbox.kind(),
        evidence.salt(),
        path_var.as_deref(),
    );
    let checks = build_free_checks_for_targets(
        &targets,
        &free_outcomes,
        &paths.home_dir,
        sandbox.agy_state_dir(),
        opencode_key.as_deref(),
    );
    let mut account_errs = Vec::new();
    for chk in &checks {
        if let Err(reason) = &chk.account_ok {
            if !account_errs.contains(reason) {
                account_errs.push(reason.clone());
            }
        }
    }
    if !account_errs.is_empty() {
        let joined = account_errs.join("\n");
        let refs: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
        return Err(LoopError::err(redact_loop_text(&joined, &refs)));
    }

    let planned = plan_preflight(
        &targets,
        &checks,
        &evidence,
        &cfg.preflight,
        &identities,
        unix_now_ms(),
    );

    println!(
        "Workflow '{workflow_name}' válido (sandbox: {}).",
        sandbox.kind()
    );
    if exposures.is_empty() {
        println!("Exposições do sandbox: nenhuma adicional detectada.");
    } else {
        println!("Exposições do sandbox:");
        for exp in &exposures {
            println!("  - {exp}");
        }
    }
    println!("Modelos resolvidos:");
    for st in &resolution.stages {
        println!(
            "  {} ({}): {} [{}]",
            st.stage_id,
            st.role,
            st.choice.display(),
            st.source
        );
    }
    println!(
        "Pré-teste (modo: {}, validade: {}h):",
        cfg.preflight.mode.as_str(),
        cfg.preflight.valid_hours
    );
    for item in &planned {
        let summary = match &item.action {
            PlanAction::Evidence => "evidência recente válida (não chama modelo)".to_string(),
            PlanAction::Probe => match cfg.preflight.mode {
                PreflightMode::Always => "chamada real (modo always)".to_string(),
                _ => "chamada real (sem evidência válida)".to_string(),
            },
            PlanAction::FreeOnly => "verificação gratuita ok (não chama modelo)".to_string(),
            PlanAction::Fail(reason) => format!("FALHA: {}", for_terminal(reason, &extra)),
        };
        println!(
            "  {} (etapas: {}): {}",
            item.target.choice.display(),
            item.target.stage_ids.join(", "),
            summary
        );
    }

    if !cfg.auto_approve_in_sandbox {
        println!("Aviso: auto_approve_in_sandbox está false em ~/.config/tchelvis/loops.toml (obrigatório true para rodar).");
    }

    if live {
        if !cfg.auto_approve_in_sandbox {
            return Err(LoopError::usage(
                "auto_approve_in_sandbox = true em ~/.config/tchelvis/loops.toml é obrigatório para --live",
            ));
        }
        let probe_dir = TempDirGuard::create(paths.state_dir.join("loops").join(format!(
            ".check-live-{}-{}",
            std::process::id(),
            generate_run_id()
        )))
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
        let probes = run_preflight_with_options(
            &targets,
            sandbox.as_ref(),
            probe_dir.path(),
            &cancel,
            &clock,
            opencode_key.as_deref(),
        );
        if cancel.load(Ordering::Relaxed) {
            return Err(LoopError::cancelled("pré-teste cancelado por sinal"));
        }
        match probes {
            Ok(records) => {
                record_probe_evidence(
                    &mut evidence,
                    &records,
                    &free_outcomes,
                    &paths.home_dir,
                    sandbox.agy_state_dir(),
                    opencode_key.as_deref(),
                    sandbox.kind(),
                    unix_now_ms(),
                    &extra,
                );
                println!("Pré-teste ao vivo concluído ({} modelo(s)):", records.len());
                for rec in records {
                    println!(
                        "  {}: {} — {}ms",
                        rec.provider,
                        rec.model.as_deref().unwrap_or("(padrão)"),
                        rec.duration_ms
                    );
                }
            }
            Err(failures) => {
                discard_failed_probes(&mut evidence, &failures, &extra);
                return Err(LoopError::err(format_probe_failures(&failures, &extra)));
            }
        }
    } else if let Some(fail_msg) = format_plan_failures(&planned, &extra) {
        return Err(LoopError::err(fail_msg));
    }

    Ok(())
}

async fn resolve_caller_info(require_herdr: bool) -> Result<Option<CallerInfo>, LoopError> {
    let raw_term = std::env::var("HC_TERMINAL_ID").unwrap_or_default();
    let term_id = raw_term.trim();
    if term_id.is_empty() {
        if require_herdr {
            return Err(LoopError::usage(
                "--detach requer execução dentro de um terminal Herdr (HC_TERMINAL_ID ausente)",
            ));
        }
        return Ok(None);
    }

    let herdr = HerdrClient::default();
    let mut rpc = None;
    match herdr.get_terminal(&mut rpc, term_id).await {
        Ok(term) => match term.created_ms {
            Some(created_ms) => Ok(Some(CallerInfo {
                terminal: term.address,
                created_ms,
            })),
            None if require_herdr => Err(LoopError::usage(
                "--detach requer Herdr com 'created_ms' válido para o terminal atual",
            )),
            None => Ok(None),
        },
        Err(e) if require_herdr => Err(LoopError::err(format!(
            "--detach requer Herdr ('hc') operacional: {e:#}"
        ))),
        Err(_) => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_run(
    paths: &CockpitPaths,
    workflow_name: String,
    prompt_arg: Option<String>,
    input_file: Option<PathBuf>,
    from_head: bool,
    max_cycles: Option<u32>,
    use_overrides_raw: Vec<String>,
    detach: bool,
    verbose: bool,
) -> Result<i32, LoopError> {
    if let Some(mc) = max_cycles {
        if !(1..=20).contains(&mc) {
            return Err(LoopError::usage("--max-cycles deve ficar entre 1 e 20"));
        }
    }

    let input_prompt = match (prompt_arg, input_file) {
        (Some(p), None) => p,
        (None, Some(path)) => read_regular_file_nofollow(&path, MAX_INPUT_FILE_BYTES)
            .map_err(|e| LoopError::usage(format!("{e:#}")))?,
        (None, None) => {
            return Err(LoopError::usage(
                "informe [prompt] ou --input-file <arquivo>",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(LoopError::usage(
                "[prompt] e --input-file são mutuamente exclusivos",
            ));
        }
    };
    if input_prompt.trim().is_empty() {
        return Err(LoopError::usage("a tarefa (prompt) não pode estar vazia"));
    }

    let use_overrides = parse_use_overrides(&use_overrides_raw)?;

    let cfg = load_loops_config_required(paths).map_err(|e| LoopError::usage(format!("{e:#}")))?;
    if !cfg.auto_approve_in_sandbox {
        return Err(LoopError::usage(
            "auto_approve_in_sandbox = true é obrigatório em ~/.config/tchelvis/loops.toml (revise os limites do sandbox antes de ativar)",
        ));
    }

    let repo = find_git_repo_root().map_err(|e| LoopError::usage(format!("{e:#}")))?;
    let (unresolved, origin) =
        load_workflow_unresolved(&workflow_name, Some(&repo), paths, max_cycles)
            .map_err(|e| LoopError::usage(format!("{e:#}")))?;
    let project_models =
        load_project_models_from_repo(&repo).map_err(|e| LoopError::usage(format!("{e:#}")))?;
    let without_use = resolve_workflow(
        unresolved.clone(),
        origin,
        &[],
        project_models.as_ref(),
        &cfg.defaults,
    );
    let resolution = resolve_workflow(
        unresolved,
        origin,
        &use_overrides,
        project_models.as_ref(),
        &cfg.defaults,
    )
    .map_err(|errs| LoopError::usage(errs.join("\n")))?;
    // What is confirmed is the repository's choices, the same ones `models confirm` records,
    // whatever `--use` replaces in this run.
    let to_confirm = without_use.as_ref().unwrap_or(&resolution);

    let roles = load_roles_for_workflow(&resolution.workflow, Some(&repo), paths)
        .map_err(|e| LoopError::usage(format!("{e:#}")))?;

    let mut run_id = generate_run_id();
    while repo.join(".loops/runs").join(&run_id).exists()
        || paths.state_dir.join("loops").join(&run_id).exists()
    {
        run_id = generate_run_id();
    }
    let run_dir = repo.join(".loops/runs").join(&run_id);
    let workspace_path = paths
        .state_dir
        .join("loops")
        .join(&run_id)
        .join("workspace");
    let ev_path = evidence_store_path(paths);

    if let SandboxConfig::Bwrap { ref home_ro } = cfg.sandbox {
        check_home_ro_does_not_reach(
            &paths.home_dir,
            home_ro,
            &[&run_dir, &workspace_path, &ev_path],
        )
        .map_err(|e| LoopError::usage(format!("{e:#}")))?;
    }

    // A foreground run prints its result: there is nobody to wake.
    let caller = if detach {
        resolve_caller_info(true).await?
    } else {
        None
    };

    // Check max_concurrent in orch.db
    let store =
        OrchStore::open(&paths.orch_db_file()).map_err(|e| LoopError::err(format!("{e:#}")))?;
    let active_count = store
        .list_loops()
        .map_err(|e| LoopError::err(format!("{e:#}")))?
        .into_iter()
        .filter(is_active)
        .count();
    if active_count >= cfg.max_concurrent as usize {
        return Err(LoopError::err(format!(
            "limite de concorrência atingido ({active_count}/{} loops em execução)",
            cfg.max_concurrent
        )));
    }

    // 1. Sandbox check
    ensure_sandbox_prereqs(&cfg)?;
    let sandbox = build_sandbox(&cfg, paths);
    let sandbox_probe_dir = TempDirGuard::create(
        paths
            .state_dir
            .join("loops")
            .join(format!(".sandbox-check-{run_id}")),
    )
    .map_err(|e| LoopError::err(format!("{e:#}")))?;
    let exposures = sandbox.check(sandbox_probe_dir.path()).map_err(|e| {
        LoopError::err(format!(
            "verificação do sandbox '{}' falhou: {e:#}",
            sandbox.kind()
        ))
    })?;
    drop(sandbox_probe_dir);

    // 2. Model confirmation
    let confirm_file = paths.state_dir.join("loops/confirmed.json");
    if resolution.needs_confirmation()
        && !is_confirmed(&confirm_file, &repo, to_confirm)
            .map_err(|e| LoopError::err(format!("{e:#}")))?
    {
        if detach || !std::io::stdin().is_terminal() {
            return Err(LoopError::err(format!(
                "o workflow '{workflow_name}' usa escolhas de modelo definidas no repositório.\n\
                 Confirme antes com:\n  tchelvis loop models confirm {workflow_name}"
            )));
        }
        eprintln!("Modelos escolhidos para '{workflow_name}':");
        for st in &to_confirm.stages {
            eprintln!(
                "  {} ({}): {} [{}]",
                st.stage_id,
                st.role,
                st.choice.display(),
                st.source
            );
        }
        eprint!("Confirmar execução com esses modelos do repositório? [y/N]: ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "y" | "yes" | "s" | "sim"
            )
        {
            return Err(LoopError::err(
                "execução recusada sem confirmação dos modelos",
            ));
        }
        record_confirmation(&confirm_file, &repo, to_confirm)
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
    }

    // 3. Resolve OPENCODE_API_KEY + secrets for redaction
    let opencode_key = opencode_key_for(&resolution.workflow);
    let extra_secrets = collect_extra_secrets(opencode_key.as_deref());
    let pi_install = pi_managed_install(&paths.home_dir, std::env::var_os("PATH").as_deref());
    for note in pi_check_notes(&resolution.workflow, &pi_install).map_err(LoopError::err)? {
        eprint_redacted(&note, &extra_secrets);
    }

    // 4. Read-only git check BEFORE preflight so dirty repo runs zero CLIs
    let branch = format!("loop/{}-{run_id}", resolution.workflow.name);
    let repo_info = GitWorkspace::preflight_check_repo(&repo, &branch, from_head)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;

    // 5. Three-level preflight: Level 1 (free CLI checks) -> Level 2 (evidence store) -> Level 3 (lean probes)
    let cancel = install_cancel_signal_flag();
    let targets = probe_targets(&resolution.workflow);
    let providers = distinct_providers(&targets);
    let preflight_dir = TempDirGuard::create(
        paths
            .state_dir
            .join("loops")
            .join(format!(".preflight-{run_id}")),
    )
    .map_err(|e| LoopError::err(format!("{e:#}")))?;
    let clock = SystemClock::new();

    let pi_providers = pi_providers_of(&targets);
    let free_outcomes = run_free_checks_with_options(
        &providers,
        sandbox.as_ref(),
        preflight_dir.path(),
        &paths.home_dir,
        &cancel,
        &clock,
        opencode_key.as_deref(),
        &pi_providers,
    )
    .map_err(|_| LoopError::cancelled("execução cancelada durante o pré-teste"))?;

    let mut evidence = EvidenceStore::open(&ev_path);
    if let Some(warn) = evidence.warning() {
        eprint_redacted(&format!("aviso: {warn}"), &extra_secrets);
    }

    let path_var = std::env::var_os("PATH");
    let initial_identities = build_cli_identities(
        &providers,
        &free_outcomes,
        &paths.home_dir,
        sandbox.agy_state_dir(),
        opencode_key.as_deref(),
        sandbox.kind(),
        evidence.salt(),
        path_var.as_deref(),
    );
    let free_checks = build_free_checks_for_targets(
        &targets,
        &free_outcomes,
        &paths.home_dir,
        sandbox.agy_state_dir(),
        opencode_key.as_deref(),
    );
    let planned = plan_preflight(
        &targets,
        &free_checks,
        &evidence,
        &cfg.preflight,
        &initial_identities,
        unix_now_ms(),
    );
    if let Some(fail_msg) = format_plan_failures(&planned, &extra_secrets) {
        return Err(LoopError::err(fail_msg));
    }

    let to_probe: Vec<ProbeTarget> = planned
        .iter()
        .filter(|p| matches!(p.action, PlanAction::Probe))
        .map(|p| p.target.clone())
        .collect();

    if verbose {
        eprintln!(
            "Pré-teste: {} modelo(s) via chamada real ({} no total)...",
            to_probe.len(),
            targets.len()
        );
    }

    let probed_records = if to_probe.is_empty() {
        Vec::new()
    } else {
        let preflight_res = run_preflight_with_options(
            &to_probe,
            sandbox.as_ref(),
            preflight_dir.path(),
            &cancel,
            &clock,
            opencode_key.as_deref(),
        );
        if cancel.load(Ordering::Relaxed) {
            return Err(LoopError::cancelled(
                "execução cancelada durante o pré-teste",
            ));
        }
        match preflight_res {
            Ok(records) => {
                record_probe_evidence(
                    &mut evidence,
                    &records,
                    &free_outcomes,
                    &paths.home_dir,
                    sandbox.agy_state_dir(),
                    opencode_key.as_deref(),
                    sandbox.kind(),
                    unix_now_ms(),
                    &extra_secrets,
                );
                records
            }
            Err(failures) => {
                discard_failed_probes(&mut evidence, &failures, &extra_secrets);
                return Err(LoopError::err(format_probe_failures(
                    &failures,
                    &extra_secrets,
                )));
            }
        }
    };
    drop(preflight_dir);
    let preflight_records = assemble_preflight_records(&planned, probed_records);
    let frozen_salt = *evidence.salt();
    let frozen_identities = build_cli_identities(
        &providers,
        &free_outcomes,
        &paths.home_dir,
        sandbox.agy_state_dir(),
        opencode_key.as_deref(),
        sandbox.kind(),
        &frozen_salt,
        path_var.as_deref(),
    );

    // 6. Side effects: .loops/runs/.gitignore, run_dir, workspace clone, frozen.json, orch.db row
    let _ =
        ensure_dir_no_symlink(&repo, &[".loops"]).map_err(|e| LoopError::err(format!("{e:#}")))?;
    let runs_root = ensure_dir_no_symlink(&repo, &[".loops", "runs"])
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
    let _ = write_if_absent_nofollow(&runs_root.join(".gitignore"), "*\n", 0o644)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;

    if detach {
        // Without the daemon the caller would never be woken.
        ensure_daemon(paths).await?;
    }
    let mut created = CreatedRun {
        paths: vec![run_dir.clone(), paths.state_dir.join("loops").join(&run_id)],
        keep: false,
    };
    let ws = GitWorkspace::create_in(repo_info, &run_dir, &workspace_path)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
    let ws_meta = ws.to_frozen();
    let base_commit = ws.base_commit().to_string();

    let frozen = FrozenRunConfig {
        run_id: run_id.clone(),
        workflow: resolution.workflow.clone(),
        roles: roles.clone(),
        input_prompt,
        sandbox_kind: sandbox.kind().to_string(),
        sandbox: cfg.sandbox.clone(),
        env_pass: cfg.env_pass.clone(),
        sandbox_exposures: exposures,
        branch: branch.clone(),
        base_commit: Some(base_commit),
        caller: caller.clone(),
        model_sources: resolution.model_sources(),
        preflight: preflight_records,
        evidence_identities: frozen_identities,
        evidence_salt: Some(hex_encode(&frozen_salt)),
        workspace: ws_meta,
        path: std::env::var("PATH").ok(),
    };
    let frozen_bytes = serde_json::to_vec_pretty(&frozen)
        .map_err(|e| LoopError::err(format!("falha ao serializar frozen.json: {e:#}")))?;
    write_private_atomic(&run_dir.join("frozen.json"), &frozen_bytes)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;

    let now_ms = unix_now_ms();
    let record = LoopRunRecord {
        run_id: run_id.clone(),
        dir: run_dir.to_string_lossy().into_owned(),
        workflow: frozen.workflow.name.clone(),
        status: "RUNNING".to_string(),
        caller_terminal: caller.as_ref().map(|c| c.terminal.clone()),
        caller_created_ms: caller.as_ref().map(|c| c.created_ms),
        engine_pid: None,
        engine_start_ticks: None,
        orch_run_id: None,
        notify_state: if caller.is_some() {
            "pending".to_string()
        } else {
            "none".to_string()
        },
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    let registered = store
        .register_loop_limited(&record, cfg.max_concurrent as usize, &is_active)
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
    if !registered {
        return Err(LoopError::err(format!(
            "limite de concorrência atingido ({} loops em execução)",
            cfg.max_concurrent
        )));
    }
    created.keep = true;

    if detach {
        let caller_term = caller
            .as_ref()
            .map(|c| c.terminal.as_str())
            .ok_or_else(|| LoopError::usage("--detach requer caller Herdr"))?;
        // The same binary, whatever `PATH` the Herdr terminal has.
        let exe = std::env::current_exe()
            .map_err(|e| LoopError::err(format!("falha ao localizar binário tchelvis: {e:#}")))?
            .to_string_lossy()
            .into_owned();
        let herdr = HerdrClient::default();
        let argv = vec![exe, "loop".to_string(), "engine".to_string()];
        let term_name = loop_terminal_name(&run_id);
        if let Err(e) = herdr
            .run_worker_named(
                caller_term,
                &repo,
                &[("cockpit.loop_run", run_id.as_str())],
                &argv,
                &run_id,
                term_name.as_deref(),
            )
            .await
        {
            let _ = store.mark_loop_launch_failed(&run_id, unix_now_ms());
            let refs: Vec<&str> = extra_secrets.iter().map(|s| s.as_str()).collect();
            return Err(LoopError::err(redact_loop_text(
                &format!("falha ao iniciar terminal do loop via Herdr: {e:#}"),
                &refs,
            )));
        }
        println!("{run_id}");
        return Ok(EXIT_PASS);
    }

    eprintln!("RUN_ID: {run_id}");
    run_frozen_engine_with_cancel(&run_dir, paths, cancel, verbose).await
}

fn loop_terminal_name(run_id: &str) -> Option<String> {
    let candidate = format!("loop-{run_id}");
    let valid = (1..=32).contains(&candidate.len())
        && candidate.bytes().enumerate().all(|(idx, b)| {
            let is_alnum = b.is_ascii_lowercase() || b.is_ascii_digit();
            if idx == 0 {
                is_alnum
            } else {
                is_alnum || matches!(b, b'-' | b'_' | b'.')
            }
        });
    valid.then_some(candidate)
}

/// Only registered runs: files under `.loops/runs` may come with a cloned repository.
fn find_run_dir(
    run_id: Option<&str>,
    paths: &CockpitPaths,
) -> Result<(String, PathBuf), LoopError> {
    if let Some(id) = run_id {
        let rec = registered_run(id, paths)?;
        return Ok((rec.run_id, PathBuf::from(rec.dir)));
    }
    let loops = OrchStore::open(&paths.orch_db_file())
        .and_then(|store| store.list_loops())
        .map_err(|e| LoopError::err(format!("{e:#}")))?;
    loops
        .iter()
        .rev()
        .find(|r| Path::new(&r.dir).is_dir())
        .or_else(|| loops.last())
        .map(|r| (r.run_id.clone(), PathBuf::from(&r.dir)))
        .ok_or_else(|| LoopError::err("nenhuma execução encontrada"))
}

/// Removes what a run created when it cannot start after all.
struct CreatedRun {
    paths: Vec<PathBuf>,
    keep: bool,
}

impl Drop for CreatedRun {
    fn drop(&mut self) {
        if !self.keep {
            for path in &self.paths {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

async fn ensure_daemon(paths: &CockpitPaths) -> Result<(), LoopError> {
    #[cfg(debug_assertions)]
    if std::env::var_os("TCHELVIS_TEST_NO_DAEMON").is_some() {
        return Ok(());
    }
    crate::daemon::ensure_daemon_running(paths)
        .await
        .map_err(|e| {
            LoopError::err(format!(
                "o daemon que avisa o fim do loop não iniciou: {e:#}"
            ))
        })
}

/// Whether a registered loop is really running: a dead engine takes no slot and blocks no cleanup.
fn is_active(rec: &LoopRunRecord) -> bool {
    if FlockGuard::is_locked(&Path::new(&rec.dir).join("engine.lock")) {
        return true;
    }
    rec.status == "RUNNING"
        && match rec.engine_pid {
            Some(pid) => LinuxProcTable.is_alive(pid, rec.engine_start_ticks),
            None => unix_now_ms().saturating_sub(rec.created_at_ms) <= ENGINE_START_GRACE_MS,
        }
}

fn registered_run(run_id: &str, paths: &CockpitPaths) -> Result<LoopRunRecord, LoopError> {
    validate_run_id(run_id).map_err(LoopError::usage)?;
    OrchStore::open(&paths.orch_db_file())
        .and_then(|store| store.get_loop(run_id))
        .map_err(|e| LoopError::err(format!("{e:#}")))?
        .ok_or_else(|| LoopError::err(format!("execução '{run_id}' não encontrada")))
}

async fn run_frozen_engine_with_cancel(
    run_dir: &Path,
    paths: &CockpitPaths,
    cancel: Arc<AtomicBool>,
    verbose: bool,
) -> Result<i32, LoopError> {
    let lock_path = run_dir.join("engine.lock");
    let _lock =
        FlockGuard::try_lock_exclusive(&lock_path).map_err(|e| LoopError::err(format!("{e:#}")))?;

    let frozen_raw = read_regular_file_nofollow(&run_dir.join("frozen.json"), MAX_FROZEN_BYTES)
        .map_err(|e| LoopError::err(format!("falha ao ler frozen.json: {e:#}")))?;
    let frozen: FrozenRunConfig = serde_json::from_str(&frozen_raw)
        .map_err(|e| LoopError::err(format!("frozen.json inválido: {e:#}")))?;

    if let Some(path) = &frozen.path {
        std::env::set_var("PATH", path);
    }

    if let Ok(store) = OrchStore::open(&paths.orch_db_file()) {
        let (pid, ticks) = current_process_identity();
        let _ = store.update_loop_engine_identity(&frozen.run_id, pid, ticks, unix_now_ms());
    }

    let opencode_key = opencode_key_for(&frozen.workflow);
    let extra_secrets = collect_extra_secrets(opencode_key.as_deref());

    if verbose {
        eprintln!(
            "Iniciando loop {} ({}) em {}...",
            frozen.run_id, frozen.workflow.name, frozen.sandbox_kind
        );
    }

    let run_dir_owned = run_dir.to_path_buf();
    let cancel_for_thread = cancel.clone();
    let secrets_for_thread = extra_secrets.clone();
    let orch_db_path = paths.orch_db_file();
    let ev_path = evidence_store_path(paths);
    let sandbox_paths = paths.clone();
    let final_state = tokio::task::spawn_blocking(move || -> RunState {
        let sandbox: Box<dyn Sandbox + Sync> = match &frozen.sandbox {
            SandboxConfig::Inherited => Box::new(InheritedSandbox::new(frozen.env_pass.clone())),
            SandboxConfig::Bwrap { home_ro } => {
                Box::new(bwrap_sandbox(home_ro, &frozen.env_pass, &sandbox_paths))
            }
        };
        let clock = SystemClock::new();
        let runner = ProcessStageRunner::new(sandbox.as_ref(), &clock)
            .with_opencode_api_key(opencode_key)
            .with_extra_secrets(secrets_for_thread.clone());
        let ws = GitWorkspace::from_frozen(frozen.workspace);
        let cfg = EngineConfig {
            run_id: frozen.run_id,
            workflow: frozen.workflow,
            roles: frozen.roles,
            run_dir: run_dir_owned,
            input_prompt: frozen.input_prompt,
            sandbox_kind: frozen.sandbox_kind,
            sandbox_exposures: frozen.sandbox_exposures,
            branch: frozen.branch,
            base_commit: frozen.base_commit,
            caller: frozen.caller,
            cancel: cancel_for_thread,
        };
        let recorder = StoreEvidenceRecorder {
            path: ev_path,
            salt: frozen.evidence_salt.as_deref().and_then(hex_decode_32),
            identities: frozen.evidence_identities,
        };
        Engine::new(cfg, ws, runner, SystemClock::new())
            .with_model_sources(frozen.model_sources)
            .with_preflight(frozen.preflight)
            .with_evidence_recorder(recorder)
            .with_extra_secrets(secrets_for_thread)
            .with_on_final(move |st| {
                if let Ok(store) = OrchStore::open(&orch_db_path) {
                    let _ = store.record_loop_final(&st.run_id, st.status.as_str(), unix_now_ms());
                }
            })
            .run()
    })
    .await
    .map_err(|e| LoopError::err(format!("falha interna no motor do loop: {e}")))?;

    if let Ok(store) = OrchStore::open(&paths.orch_db_file()) {
        let _ = store.record_loop_final(
            &final_state.run_id,
            final_state.status.as_str(),
            unix_now_ms(),
        );
    }
    let req = IpcRequest {
        op: IpcOp::OrchWake,
        terminal: None,
        include_all: false,
    };
    let _ = crate::daemon::query_socket_once::<Value>(&paths.socket_file(), &req).await;

    println!("{}", format_result(&final_state));
    Ok(final_state.status.exit_code())
}

async fn cmd_engine(paths: &CockpitPaths, run_id: String) -> Result<i32, LoopError> {
    validate_run_id(&run_id).map_err(LoopError::usage)?;
    let rec = registered_run(&run_id, paths)?;
    if rec.status != "RUNNING" {
        return Err(LoopError::err(format!(
            "execução '{run_id}' já terminou ({})",
            rec.status
        )));
    }
    let run_dir = PathBuf::from(&rec.dir);
    if rec.engine_pid.is_some() && !FlockGuard::is_locked(&run_dir.join("engine.lock")) {
        return Err(LoopError::err(format!(
            "o motor da execução '{run_id}' já foi iniciado e não pode recomeçar"
        )));
    }
    let cancel = install_cancel_signal_flag();
    run_frozen_engine_with_cancel(&run_dir, paths, cancel, false).await
}

fn run_dir_missing(dir: &Path) -> bool {
    matches!(
        fs::symlink_metadata(dir),
        Err(ref err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

fn removed_records_message(run_id: &str, status: &str) -> String {
    format!(
        "os registros da execução '{run_id}' foram removidos (loop clean); status registrado: {status}"
    )
}

fn cmd_status(paths: &CockpitPaths, run_id: Option<&str>, json: bool) -> Result<(), LoopError> {
    let (resolved_id, run_dir) = find_run_dir(run_id, paths)?;
    match load_state(&run_dir) {
        Ok(state) => {
            if json {
                let out = serde_json::to_string_pretty(&state)
                    .map_err(|e| LoopError::err(format!("{e:#}")))?;
                println!("{out}");
            } else {
                println!(
                    "STATUS: {} | LOOP: {} | CICLOS: {}/{}",
                    state.status, state.workflow, state.current_cycle, state.max_cycles
                );
                println!("RUN_ID: {}", state.run_id);
                if let Some(branch) = &state.branch {
                    println!("BRANCH: {branch}");
                }
                if let Some(reason) = &state.reason {
                    println!("MOTIVO: {reason}");
                }
                println!("ETAPAS EXECUTADAS: {}", state.executions.len());
            }
            Ok(())
        }
        Err(_) => {
            let store = OrchStore::open(&paths.orch_db_file())
                .map_err(|e| LoopError::err(format!("{e:#}")))?;
            let Some(rec) = store
                .get_loop(&resolved_id)
                .map_err(|e| LoopError::err(format!("{e:#}")))?
            else {
                return Err(LoopError::err(format!(
                    "estado da execução '{resolved_id}' ainda indisponível"
                )));
            };
            let removed = run_dir_missing(&run_dir);
            if json {
                let mut val = serde_json::json!({
                    "run_id": rec.run_id,
                    "workflow": rec.workflow,
                    "status": rec.status,
                    "notify_state": rec.notify_state,
                    "dir": rec.dir,
                });
                if removed {
                    let msg = removed_records_message(&rec.run_id, &rec.status);
                    if let Some(obj) = val.as_object_mut() {
                        obj.insert("records_removed".to_string(), Value::Bool(true));
                        obj.insert("message".to_string(), Value::String(msg));
                    }
                }
                println!("{}", serde_json::to_string_pretty(&val).unwrap_or_default());
            } else {
                println!("STATUS: {} | LOOP: {}", rec.status, rec.workflow);
                println!("RUN_ID: {}", rec.run_id);
                if removed {
                    println!(
                        "AVISO: {}",
                        removed_records_message(&rec.run_id, &rec.status)
                    );
                }
            }
            Ok(())
        }
    }
}

fn cmd_result(paths: &CockpitPaths, run_id: Option<&str>) -> Result<(), LoopError> {
    let (id, run_dir) = find_run_dir(run_id, paths)?;
    match load_state(&run_dir) {
        Ok(state) => println!("{}", format_result(&state)),
        // The engine ended before writing anything (or disk files were cleaned): the record still says how.
        Err(_) => {
            let rec = registered_run(&id, paths)?;
            let summary = if run_dir_missing(&run_dir) {
                removed_records_message(&rec.run_id, &rec.status)
            } else {
                "sem estado gravado; o motor não chegou a iniciar".to_string()
            };
            println!(
                "STATUS: {} | LOOP: {} | CICLOS: 0/0\nRUN_ID: {}\nBRANCH: (nenhuma)\nARQUIVOS: (nenhum)\nRESUMO: {}",
                rec.status, rec.workflow, rec.run_id, summary
            );
        }
    }
    Ok(())
}

/// Values to hide when showing files of a run.
fn secrets_of_run(run_dir: &Path) -> Vec<String> {
    let key = read_regular_file_nofollow(&run_dir.join("frozen.json"), MAX_FROZEN_BYTES)
        .ok()
        .and_then(|raw| serde_json::from_str::<FrozenRunConfig>(&raw).ok())
        .and_then(|frozen| opencode_key_for(&frozen.workflow));
    collect_extra_secrets(key.as_deref())
}

fn cmd_logs(
    paths: &CockpitPaths,
    run_id: Option<&str>,
    stage_filter: Option<&str>,
) -> Result<(), LoopError> {
    let (id, run_dir) = find_run_dir(run_id, paths)?;
    if run_dir_missing(&run_dir) {
        let rec = registered_run(&id, paths)?;
        return Err(LoopError::err(removed_records_message(
            &rec.run_id,
            &rec.status,
        )));
    }
    let logs_dir = run_dir.join("logs");
    if !logs_dir.is_dir() {
        println!("Nenhum log encontrado em {}.", logs_dir.display());
        return Ok(());
    }
    let mut entries: Vec<String> = fs::read_dir(&logs_dir)
        .map_err(|e| LoopError::err(format!("{e:#}")))?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".log"))
        .collect();
    entries.sort();
    if let Some(filter) = stage_filter {
        entries.retain(|name| {
            name.strip_suffix(".log")
                .map(|stem| stem.split('-').skip(2).collect::<Vec<_>>().join("-"))
                .as_deref()
                == Some(filter)
                || name.contains(filter)
        });
    }
    if entries.is_empty() {
        println!("Nenhum log correspondente encontrado.");
        return Ok(());
    }
    let extra = secrets_of_run(&run_dir);
    for name in entries {
        println!("=== {} ===", for_terminal(&name, &extra));
        let raw = read_regular_bytes_nofollow(&logs_dir.join(&name), MAX_LOG_PRINT_BYTES)
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
        println!("{}", for_terminal(&String::from_utf8_lossy(&raw), &extra));
    }
    Ok(())
}

fn cmd_report(paths: &CockpitPaths, run_id: Option<&str>) -> Result<(), LoopError> {
    let (id, run_dir) = find_run_dir(run_id, paths)?;
    if run_dir_missing(&run_dir) {
        let rec = registered_run(&id, paths)?;
        return Err(LoopError::err(removed_records_message(
            &rec.run_id,
            &rec.status,
        )));
    }
    let extra = secrets_of_run(&run_dir);
    let extra_refs: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
    let report_path = run_dir.join("report.md");
    if report_path.exists() {
        let md = read_regular_file_nofollow(&report_path, MAX_LOG_PRINT_BYTES)
            .map_err(|e| LoopError::err(format!("{e:#}")))?;
        println!("{}", for_terminal(&md, &extra));
        return Ok(());
    }
    let state = load_state(&run_dir).map_err(|e| LoopError::err(format!("{e:#}")))?;
    print!(
        "{}",
        render_report_with_secrets(&state, &run_dir, &extra_refs)
    );
    Ok(())
}

fn resolve_stop_run_id(paths: &CockpitPaths, run_id: Option<&str>) -> Result<String, LoopError> {
    if let Some(id) = run_id {
        return Ok(id.to_string());
    }
    let repo = find_git_repo_root().map_err(|e| LoopError::usage(format!("{e:#}")))?;
    let db_path = paths.orch_db_file();
    if !db_path.exists() {
        return Err(LoopError::err(
            "nenhum loop ativo no repositório atual (0 em execução)",
        ));
    }
    let store = OrchStore::open(&db_path).map_err(|e| LoopError::err(format!("{e:#}")))?;
    // Exact, as in `clean`: following a link in `.loops` would reach another repository's loops.
    let runs = repo.join(".loops/runs");
    let active: Vec<LoopRunRecord> = store
        .list_loops()
        .map_err(|e| LoopError::err(format!("{e:#}")))?
        .into_iter()
        .filter(|rec| Path::new(&rec.dir).parent() == Some(runs.as_path()) && is_active(rec))
        .collect();
    match active.len() {
        0 => Err(LoopError::err(
            "nenhum loop ativo no repositório atual (0 em execução)",
        )),
        1 => Ok(active[0].run_id.clone()),
        _ => {
            let ids = active
                .iter()
                .map(|r| format!("{} ({})", r.run_id, r.workflow))
                .collect::<Vec<_>>()
                .join(", ");
            Err(LoopError::err(format!(
                "múltiplos loops ativos no repositório atual: {ids}; especifique o RUN_ID em 'tchelvis loop stop <RUN_ID>'"
            )))
        }
    }
}

fn cmd_stop(paths: &CockpitPaths, run_id: Option<&str>) -> Result<(), LoopError> {
    let resolved_id = resolve_stop_run_id(paths, run_id)?;
    let run_id = resolved_id.as_str();
    let rec = registered_run(run_id, paths)?;
    if rec.status != "RUNNING" {
        println!("Loop '{run_id}' já finalizado ({}).", rec.status);
        return Ok(());
    }
    // Only the identity the engine registered: the pid in `state.json` could be anyone's.
    let (Some(pid), Some(ticks)) = (rec.engine_pid, rec.engine_start_ticks) else {
        return Err(LoopError::err(format!(
            "o motor do loop '{run_id}' ainda não iniciou"
        )));
    };
    if !LinuxProcTable.is_alive(pid, Some(ticks)) {
        return Err(LoopError::err(format!(
            "motor do loop '{run_id}' (PID {pid}) não está mais em execução"
        )));
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(LoopError::err(format!(
            "falha ao enviar SIGINT para PID {pid}: {err}"
        )));
    }
    println!("Sinal de cancelamento enviado ao loop '{run_id}' (PID {pid}).");
    Ok(())
}

/// The run's own directory, if it is exactly `<repo>/.loops/runs/<id>` with no link on the way.
fn removable_run_dir(rec: &LoopRunRecord) -> Option<PathBuf> {
    let dir = PathBuf::from(&rec.dir);
    let repo = dir.parent()?.parent()?.parent()?;
    let expected = fs::canonicalize(repo)
        .ok()?
        .join(".loops/runs")
        .join(&rec.run_id);
    (validate_run_id(&rec.run_id).is_ok() && fs::canonicalize(&dir).ok()? == expected)
        .then_some(expected)
}

/// The directory of the run's workspace, if it is a real directory.
fn removable_workspace(run_id: &str, paths: &CockpitPaths) -> Option<PathBuf> {
    let dir = paths.state_dir.join("loops").join(run_id);
    fs::symlink_metadata(&dir)
        .is_ok_and(|meta| meta.is_dir())
        .then_some(dir)
}

fn cmd_clean(paths: &CockpitPaths, run_id: Option<&str>, yes: bool) -> Result<(), LoopError> {
    let records = match run_id {
        Some(id) => {
            let rec = registered_run(id, paths)?;
            if is_active(&rec) {
                return Err(LoopError::err(format!(
                    "execução '{id}' está ativa e não pode ser removida (use 'tchelvis loop stop {id}' antes)"
                )));
            }
            vec![rec]
        }
        None => {
            let repo = find_git_repo_root().map_err(|e| LoopError::usage(format!("{e:#}")))?;
            let runs = repo.join(".loops/runs");
            OrchStore::open(&paths.orch_db_file())
                .and_then(|store| store.list_loops())
                .map_err(|e| LoopError::err(format!("{e:#}")))?
                .into_iter()
                .filter(|rec| Path::new(&rec.dir).parent() == Some(runs.as_path()))
                .filter(|rec| !is_active(rec))
                .collect()
        }
    };
    let targets: Vec<(String, Option<PathBuf>, Option<PathBuf>)> = records
        .iter()
        .map(|rec| {
            (
                rec.run_id.clone(),
                removable_run_dir(rec),
                removable_workspace(&rec.run_id, paths),
            )
        })
        .filter(|(_, run, ws)| run.is_some() || ws.is_some())
        .collect();

    if targets.is_empty() {
        println!("Nenhuma execução finalizada para limpar.");
        return Ok(());
    }

    println!("Diretórios que serão removidos (branches git serão mantidas):");
    for (id, run_dir, ws_parent) in &targets {
        println!("  [{id}]");
        for dir in [run_dir, ws_parent].into_iter().flatten() {
            println!("    - {}", dir.display());
        }
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            return Err(LoopError::err(
                "recusando limpeza sem --yes fora de terminal interativo",
            ));
        }
        eprint!("Confirmar remoção? [y/N]: ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "y" | "yes" | "s" | "sim"
            )
        {
            println!("Limpeza cancelada.");
            return Ok(());
        }
    }

    for dir in targets
        .into_iter()
        .flat_map(|(_, run, ws)| [run, ws])
        .flatten()
    {
        fs::remove_dir_all(&dir)
            .map_err(|e| LoopError::err(format!("falha ao remover {}: {e:#}", dir.display())))?;
    }
    println!("Limpeza concluída.");
    Ok(())
}

pub async fn execute_loop_command(cmd: LoopCommands, paths: &CockpitPaths) -> i32 {
    let extra = collect_extra_secrets(None);
    let result: Result<i32, LoopError> = match cmd {
        LoopCommands::Init => cmd_init(paths).map(|()| EXIT_PASS),
        LoopCommands::New { name, stages } => cmd_new(paths, &name, &stages).map(|()| EXIT_PASS),
        LoopCommands::List { runs, active } => cmd_list(paths, runs, active).map(|()| EXIT_PASS),
        LoopCommands::Check {
            workflow,
            live,
            use_overrides,
        } => cmd_check(paths, &workflow, live, &use_overrides).map(|()| EXIT_PASS),
        LoopCommands::Models { action, workflow } => {
            cmd_models(paths, action, workflow).map(|()| EXIT_PASS)
        }
        LoopCommands::Auth { action } => cmd_auth(paths, action).await.map(|()| EXIT_PASS),
        LoopCommands::Run {
            workflow,
            prompt,
            input_file,
            from_head,
            max_cycles,
            use_overrides,
            detach,
            verbose,
        } => {
            cmd_run(
                paths,
                workflow,
                prompt,
                input_file,
                from_head,
                max_cycles,
                use_overrides,
                detach,
                verbose,
            )
            .await
        }
        LoopCommands::Status { run_id, json } => {
            cmd_status(paths, run_id.as_deref(), json).map(|()| EXIT_PASS)
        }
        LoopCommands::Result { run_id } => cmd_result(paths, run_id.as_deref()).map(|()| EXIT_PASS),
        LoopCommands::Logs { run_id, stage } => {
            cmd_logs(paths, run_id.as_deref(), stage.as_deref()).map(|()| EXIT_PASS)
        }
        LoopCommands::Report { run_id } => cmd_report(paths, run_id.as_deref()).map(|()| EXIT_PASS),
        LoopCommands::Stop { run_id } => cmd_stop(paths, run_id.as_deref()).map(|()| EXIT_PASS),
        LoopCommands::Clean { run_id, yes } => {
            cmd_clean(paths, run_id.as_deref(), yes).map(|()| EXIT_PASS)
        }
        LoopCommands::Engine { run_id } => cmd_engine(paths, run_id).await,
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprint_redacted(&format!("Erro: {}", err.message), &extra);
            err.code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::workflow::{RoleDefault, RoleDefaults};
    use tempfile::TempDir;

    fn all_defaults() -> RoleDefaults {
        let mut map = BTreeMap::new();
        for role in [
            "designer", "planner", "worker", "tester", "reviewer", "critic", "auditor",
        ] {
            map.insert(
                role.to_string(),
                RoleDefault {
                    provider: Provider::Claude,
                    model: Some("sonnet".to_string()),
                },
            );
        }
        map
    }

    #[test]
    fn opencode_key_resolution_ignores_shell_startup_noise_and_handles_slow_or_failing_shells() {
        let tmp = TempDir::new().unwrap();

        // Direct env wins without invoking shell.
        assert_eq!(
            resolve_opencode_api_key_with(
                Some("env-key-123"),
                Some(std::ffi::OsStr::new("/nonexistent/shell")),
                Duration::from_millis(200),
            ),
            Some("env-key-123".to_string())
        );

        // Shell with startup noise + exported OPENCODE_API_KEY
        let noisy_shell = tmp.path().join("noisy-sh");
        crate::test_support::write_executable(
            &noisy_shell,
            "#!/usr/bin/env sh\n\
             echo 'welcome to interactive shell'\n\
             echo 'some MOTD banner line' >&2\n\
             export OPENCODE_API_KEY='shell-secret-999'\n\
             shift\n\
             sh -c \"$1\"\n",
        );
        assert_eq!(
            resolve_opencode_api_key_with(
                None,
                Some(noisy_shell.as_os_str()),
                Duration::from_secs(2),
            ),
            Some("shell-secret-999".to_string())
        );

        // Failing shell -> None ("absent", not error)
        let failing_shell = tmp.path().join("fail-sh");
        crate::test_support::write_executable(&failing_shell, "#!/usr/bin/env sh\nexit 1\n");
        assert_eq!(
            resolve_opencode_api_key_with(
                None,
                Some(failing_shell.as_os_str()),
                Duration::from_secs(1),
            ),
            None
        );

        // Slow shell -> times out cleanly -> None ("absent", not error)
        let slow_shell = tmp.path().join("slow-sh");
        crate::test_support::write_executable(&slow_shell, "#!/usr/bin/env sh\nsleep 5\n");
        let started = Instant::now();
        assert_eq!(
            resolve_opencode_api_key_with(
                None,
                Some(slow_shell.as_os_str()),
                Duration::from_millis(120),
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn loop_new_generates_preset_shapes_repeated_roles_and_unknown_roles_that_validate() {
        let defaults = all_defaults();

        let shapes: Vec<(&str, &[&str])> = vec![
            ("wrc", &["worker", "reviewer", "critic"]),
            ("dwr", &["designer", "worker", "reviewer"]),
            ("pwtc", &["planner", "worker", "tester", "critic"]),
            ("fwr", &["worker", "reviewer"]),
            (
                "custom-shape",
                &["planner", "worker", "worker", "auditor", "reviewer"],
            ),
        ];

        for (name, raw_stages) in shapes {
            let stages: Vec<String> = raw_stages.iter().map(|s| (*s).to_string()).collect();
            let (toml, roles) = build_generated_workflow_and_roles(name, &stages).unwrap();
            let unresolved = parse_workflow_unresolved(&toml).expect("valid unresolved workflow");
            let res = resolve_workflow(unresolved, WorkflowOrigin::Repo, &[], None, &defaults)
                .expect("resolves cleanly");
            assert_eq!(res.workflow.stages.len(), stages.len());

            if name == "custom-shape" {
                assert!(roles.contains_key("auditor"));
                let stage_ids: Vec<&str> =
                    res.workflow.stages.iter().map(|s| s.id.as_str()).collect();
                assert_eq!(
                    stage_ids,
                    ["planner", "worker", "worker-2", "auditor", "reviewer"]
                );
                assert_eq!(res.workflow.stages[1].on_retry, None);
                assert_eq!(res.workflow.stages[4].on_retry.as_deref(), Some("worker"));
                // First editing stage receives reads from later stages
                assert!(res.workflow.stages[1]
                    .reads
                    .contains(&"auditor.md".to_string()));
                assert!(res.workflow.stages[1]
                    .reads
                    .contains(&"review.md".to_string()));
                assert_eq!(res.workflow.stages[3].writes.as_deref(), Some("auditor.md"));
            }
        }
    }

    #[test]
    fn loop_new_rejects_invalid_shapes_and_writes_nothing() {
        // No editing stage
        assert!(build_generated_workflow_and_roles(
            "no-edit",
            &["planner".into(), "reviewer".into()]
        )
        .is_err());

        // Editing stage last
        assert!(build_generated_workflow_and_roles(
            "edit-last",
            &["planner".into(), "worker".into()]
        )
        .is_err());
        assert!(build_generated_workflow_and_roles(
            "tester-last",
            &["worker".into(), "tester".into()]
        )
        .is_err());

        // Invalid role name
        assert!(build_generated_workflow_and_roles(
            "bad-role",
            &["worker".into(), "BadRole!".into()]
        )
        .is_err());

        // Invalid workflow name
        assert!(build_generated_workflow_and_roles(
            "../escape",
            &["worker".into(), "reviewer".into()]
        )
        .is_err());
    }

    #[test]
    fn validate_run_id_rejects_path_traversal_and_special_chars() {
        assert!(validate_run_id("20261123-182500-a1b2").is_ok());
        assert!(validate_run_id("run_01").is_ok());
        for bad in ["../x", "a/b", ".hidden", "", "a\\b", "id with spaces"] {
            assert!(validate_run_id(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn only_opencode_go_needs_the_key() {
        assert!(needs_opencode_key(Some("opencode-go/hy4-flash-free")));
        for other in [
            Some("opencode/deepseek-free"),
            Some("openrouter/x:free"),
            None,
        ] {
            assert!(!needs_opencode_key(other), "{other:?}");
        }
    }

    #[test]
    fn a_shell_that_leaves_a_process_holding_its_output_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = tmp.path().join("sh");
        crate::test_support::write_executable(
            &shell,
            "#!/bin/sh\nsetsid sleep 20 &\nOPENCODE_API_KEY=k1 /bin/sh -c \"$2\"\n",
        );
        let start = Instant::now();
        let key =
            resolve_opencode_api_key_with(None, Some(shell.as_os_str()), Duration::from_secs(5));
        assert_eq!(key.as_deref(), Some("k1"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn claude_without_a_known_account_has_no_evidence_identity() {
        let tmp = TempDir::new().unwrap();
        let identity = |status: Option<ClaudeStatus>| {
            let outcomes = BTreeMap::from([(
                Provider::Claude,
                ProviderFreeOutcome {
                    provider: Provider::Claude,
                    account_ok: Ok(()),
                    models: Vec::new(),
                    list_known: status.is_some(),
                    claude_status: status,
                    pi_accounts: BTreeMap::new(),
                },
            )]);
            build_cli_identity_for_provider(
                Provider::Claude,
                &outcomes,
                tmp.path(),
                None,
                None,
                "bwrap",
                &[7u8; 32],
                None,
            )
        };
        let logged_in = |expires: Option<&str>| {
            Some(ClaudeStatus::LoggedIn {
                email: None,
                org_id: None,
                org_name: None,
                login_expires_at: expires.map(str::to_string),
            })
        };
        assert!(identity(None).is_none());
        assert!(identity(Some(ClaudeStatus::Unknown("hang".into()))).is_none());
        assert!(identity(logged_in(None)).is_none());
        let a = identity(logged_in(Some("2026-12-09T01:02:03.456Z"))).unwrap();
        let b = identity(logged_in(Some("2026-12-09T01:02:03.457Z"))).unwrap();
        assert_ne!(a.account, b.account);

        // Some logins report no expiry either: the credential file then stands for the account,
        // by its metadata only, and a rewritten file is another account.
        let cred = tmp.path().join(".claude/.credentials.json");
        fs::create_dir_all(cred.parent().unwrap()).unwrap();
        fs::write(&cred, "{}").unwrap();
        let first = identity(logged_in(None)).unwrap();
        assert_eq!(identity(logged_in(None)).unwrap(), first);
        assert_ne!(first.account, a.account);
        let swapped = cred.with_extension("new");
        fs::write(&swapped, "{}").unwrap();
        fs::rename(&swapped, &cred).unwrap();
        assert_ne!(identity(logged_in(None)).unwrap().account, first.account);
        fs::write(&cred, "").unwrap();
        assert!(identity(logged_in(None)).is_none());
        assert!(identity(None).is_none());
    }

    #[test]
    fn the_user_agy_directory_keeps_its_permissions() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".gemini/antigravity-cli");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(ensure_agy_target_dir(tmp.path()).unwrap(), dir);
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn agy_evidence_identity_tracks_chosen_credential_location() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&state).unwrap();

        let id_for = |agy_state: Option<&Path>| {
            build_cli_identity_for_provider(
                Provider::Agy,
                &BTreeMap::new(),
                &home,
                agy_state,
                None,
                "bwrap",
                &[9u8; 32],
                None,
            )
            .unwrap()
        };

        let id_none = id_for(Some(&state));

        let def_tok = home.join(crate::loops::sandbox::AGY_TOKEN_REL);
        fs::create_dir_all(def_tok.parent().unwrap()).unwrap();
        fs::write(&def_tok, "default-tok").unwrap();
        let id_default = id_for(Some(&state));
        assert_ne!(id_none.account, id_default.account);

        let loop_tok = state
            .join(crate::loops::sandbox::AGY_LOGIN_DIR_REL)
            .join(crate::loops::sandbox::AGY_TOKEN_FILE);
        fs::create_dir_all(loop_tok.parent().unwrap()).unwrap();
        fs::write(&loop_tok, "loop-tok").unwrap();
        let id_loop = id_for(Some(&state));
        assert_ne!(id_default.account, id_loop.account);
        // Another login rewrites the file: evidence of the first account is not its own.
        let swapped = loop_tok.with_extension("new");
        fs::write(&swapped, "loop-tok").unwrap();
        fs::rename(&swapped, &loop_tok).unwrap();
        let first_login = id_loop;
        let id_loop = id_for(Some(&state));
        assert_ne!(id_loop.account, first_login.account);
        assert_eq!(id_for(Some(&state)), id_loop);
        assert_ne!(id_none.account, id_loop.account);

        // Inherited (state_dir = None) ignores the loop login and still uses the default file.
        let id_inherited = id_for(None);
        assert_eq!(id_inherited.account, id_default.account);

        // Removing the default file does not change loop identity, but makes inherited absent.
        fs::remove_file(&def_tok).unwrap();
        assert_eq!(id_for(Some(&state)).account, id_loop.account);
        assert_eq!(id_for(None).account, id_none.account);
    }

    #[test]
    fn pi_free_check_maps_account_and_listing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let target = |model: &str| ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Pi,
                model: Some(model.to_string()),
            },
            stage_ids: vec!["implement".to_string()],
        };

        let not_ready = target("anthropic/claude-x");
        let mut accounts = BTreeMap::new();
        accounts.insert(
            "anthropic".to_string(),
            PiAccount::NotReady("credentials_not_configured".to_string()),
        );
        let mut outcomes = BTreeMap::new();
        outcomes.insert(
            Provider::Pi,
            ProviderFreeOutcome {
                provider: Provider::Pi,
                account_ok: Ok(()),
                models: Vec::new(),
                list_known: false,
                claude_status: None,
                pi_accounts: accounts,
            },
        );
        let checks = build_free_checks_for_targets(&[not_ready], &outcomes, &home, None, None);
        let reason = checks[0].account_ok.clone().unwrap_err();
        assert!(
            reason.contains("rode 'tchelvis loop auth login pi anthropic'"),
            "got: {reason}"
        );

        let ready_target = target("opencode-go/glm-5.3");
        let mut accounts = BTreeMap::new();
        accounts.insert("opencode-go".to_string(), PiAccount::Ready);
        let mut outcomes = BTreeMap::new();
        outcomes.insert(
            Provider::Pi,
            ProviderFreeOutcome {
                provider: Provider::Pi,
                account_ok: Ok(()),
                models: vec!["opencode-go/glm-5.3".to_string()],
                list_known: true,
                claude_status: None,
                pi_accounts: accounts,
            },
        );
        let checks = build_free_checks_for_targets(&[ready_target], &outcomes, &home, None, None);
        assert!(checks[0].account_ok.is_ok());
        assert_eq!(checks[0].listed, Listed::Yes);

        let unknown_target = target("opencode-go/glm-5.3");
        let checks =
            build_free_checks_for_targets(&[unknown_target], &BTreeMap::new(), &home, None, None);
        assert!(checks[0].account_ok.is_ok());
        assert_eq!(checks[0].listed, Listed::Unknown);
    }

    fn pi_install_fixture(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let home = tmp.path().join("home");
        let agent_bin = home.join(".pi/agent/bin");
        fs::create_dir_all(&agent_bin).unwrap();
        crate::test_support::write_executable(agent_bin.join("pi"), "#!/usr/bin/env sh\nexit 0\n");
        fs::create_dir_all(home.join(".pi/agent/install")).unwrap();
        fs::write(home.join(".pi/agent/install/current-version"), "1.2.3\n").unwrap();
        let path_dir = tmp.path().join("path-bin");
        fs::create_dir_all(&path_dir).unwrap();
        std::os::unix::fs::symlink(agent_bin.join("pi"), path_dir.join("pi")).unwrap();
        (home, path_dir)
    }

    #[test]
    fn pi_identity_changes_with_auth_file_and_version() {
        let tmp = TempDir::new().unwrap();
        let (home, path_dir) = pi_install_fixture(&tmp);
        let salt = [11u8; 32];
        let identity = || {
            build_cli_identity_for_provider(
                Provider::Pi,
                &BTreeMap::new(),
                &home,
                None,
                None,
                "bwrap",
                &salt,
                Some(path_dir.as_os_str()),
            )
        };
        assert!(identity().is_none(), "no auth.json means no account");

        let auth = home.join(".pi/agent/auth.json");
        fs::write(&auth, "{\"token\":\"one\"}").unwrap();
        let first = identity().expect("identity with auth file");
        assert!(first.account.starts_with("pi:"));
        assert!(first.executable.contains("1.2.3"), "{}", first.executable);

        fs::write(&auth, "{\"token\":\"one\",\"refreshed\":true}").unwrap();
        let second = identity().unwrap();
        assert_ne!(second.account, first.account);
        assert_eq!(second.executable, first.executable);

        fs::write(home.join(".pi/agent/install/current-version"), "2.0.0\n").unwrap();
        let third = identity().unwrap();
        assert_ne!(third.executable, second.executable);
        assert!(third.executable.contains("2.0.0"));
        assert_eq!(third.account, second.account);
    }

    #[test]
    fn pi_auth_line_texts() {
        let list = crate::loops::free_check::parse_model_list(
            Provider::Pi,
            include_str!("../../tests/fixtures/loop/free_pi_models.txt"),
        );
        let outcome = ProviderFreeOutcome {
            provider: Provider::Pi,
            account_ok: Ok(()),
            models: list,
            list_known: true,
            claude_status: None,
            pi_accounts: BTreeMap::new(),
        };
        let install = Ok(PiInstall {
            agent_dir: PathBuf::from("/home/u/.pi/agent"),
            version: "1.2.3".to_string(),
        });
        assert_eq!(
            pi_auth_line(Some(&outcome), &install),
            "pi:       34 modelo(s) de opencode-go (pi --list-models)"
        );

        let empty = ProviderFreeOutcome {
            models: Vec::new(),
            list_known: true,
            ..outcome.clone()
        };
        assert_eq!(
            pi_auth_line(Some(&empty), &install),
            "pi:       nenhum provedor com credencial (rode 'tchelvis loop auth login pi <provedor>')"
        );

        let bad = Err(crate::loops::sandbox::PI_UNRECOGNIZED.to_string());
        assert_eq!(
            pi_auth_line(Some(&outcome), &bad),
            format!("pi:       {}", crate::loops::sandbox::PI_UNRECOGNIZED)
        );
    }

    #[test]
    fn auth_login_pi_requires_provider() {
        let err = |res: Result<Vec<String>, LoopError>| res.unwrap_err().message.clone();

        assert_eq!(
            err(login_argv("pi", None)),
            "informe o provedor: tchelvis loop auth login pi <provedor>"
        );
        assert!(err(login_argv("pi", Some("Bad Name"))).contains("provedor inválido"));
        let ok = |res: Result<Vec<String>, LoopError>| res.map_err(|e| e.message).unwrap();
        assert_eq!(
            ok(login_argv("pi", Some("opencode-go"))),
            vec!["pi", "auth", "login", "opencode-go"]
        );
        assert_eq!(
            ok(login_argv("claude", None)),
            vec!["claude", "auth", "login"]
        );
        assert_eq!(
            ok(login_argv("opencode", None)),
            vec!["opencode", "auth", "login"]
        );
        assert!(err(login_argv("nope", None)).contains("CLI desconhecido 'nope'"));
        assert!(err(login_argv("nope", None)).contains("claude, agy, opencode ou pi"));
    }

    #[test]
    fn check_warns_pi_credentials_and_fails_unrecognized_install() {
        let stage = |provider: Provider| crate::loops::workflow::Stage {
            id: "s1".to_string(),
            role: "worker".to_string(),
            provider,
            model: Some("opencode-go/glm-5.3".to_string()),
            reads: Vec::new(),
            writes: Some("out.md".to_string()),
            edits: false,
            next: None,
            verdict_gate: false,
            on_pass: None,
            on_retry: None,
        };
        let wf = |provider: Provider| crate::loops::workflow::Workflow {
            name: "wf".to_string(),
            description: String::new(),
            max_cycles: 1,
            stage_timeout_secs: 60,
            stall_secs: 60,
            max_duration_secs: 600,
            stages: vec![stage(provider)],
        };
        let install = Ok(PiInstall {
            agent_dir: PathBuf::from("/home/u/.pi/agent"),
            version: "1.2.3".to_string(),
        });

        assert_eq!(
            pi_check_notes(&wf(Provider::Pi), &install).unwrap(),
            vec!["aviso: pi: a etapa vê todas as credenciais de ~/.pi/agent/auth.json".to_string()]
        );
        assert!(pi_check_notes(&wf(Provider::Claude), &install)
            .unwrap()
            .is_empty());

        let bad = Err(crate::loops::sandbox::PI_UNRECOGNIZED.to_string());
        assert_eq!(
            pi_check_notes(&wf(Provider::Pi), &bad).unwrap_err(),
            crate::loops::sandbox::PI_UNRECOGNIZED
        );
    }

    #[test]
    fn agy_free_check_respects_state_dir_for_loop_login() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let loop_tok = state
            .join(crate::loops::sandbox::AGY_LOGIN_DIR_REL)
            .join(crate::loops::sandbox::AGY_TOKEN_FILE);
        fs::create_dir_all(loop_tok.parent().unwrap()).unwrap();
        fs::write(&loop_tok, "loop-tok").unwrap();

        let targets = vec![ProbeTarget {
            choice: ModelChoice {
                provider: Provider::Agy,
                model: None,
            },
            stage_ids: vec!["s1".to_string()],
        }];
        let with_state =
            build_free_checks_for_targets(&targets, &BTreeMap::new(), &home, Some(&state), None);
        assert!(with_state[0].account_ok.is_ok());

        let without_state =
            build_free_checks_for_targets(&targets, &BTreeMap::new(), &home, None, None);
        assert_eq!(
            without_state[0]
                .account_ok
                .as_ref()
                .err()
                .map(String::as_str),
            Some(AGY_ABSENT_REASON)
        );
    }
}
