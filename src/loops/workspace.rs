use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub trait Workspace {
    fn snapshot(&mut self) -> Result<String>;
    fn restore(&mut self, tree: &str) -> Result<()>;
    fn diff(&self, tree: &str) -> Result<String>;
    fn changed_files(&self, tree: &str) -> Result<Vec<String>>;
    fn publish(&mut self, tree: &str, message: &str) -> Result<()>;
    fn path(&self) -> &Path;
}

/// What the checkout needs to materialise LFS files from the local store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsCheckout {
    /// Validated `clean`/`smudge`/`process` commands of the repository, without `--skip`.
    pub commands: BTreeMap<String, String>,
    pub storage: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RepoPreflightInfo {
    pub repo_root: PathBuf,
    pub main_git_dir: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub base_tree: String,
    pub disabled_filters: Vec<String>,
    pub lfs: Option<LfsCheckout>,
}

impl RepoPreflightInfo {
    pub fn lfs_enabled(&self) -> bool {
        self.lfs.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrozenWorkspaceMeta {
    pub main_git_dir: PathBuf,
    pub workspace_path: PathBuf,
    pub index_file: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub base_tree: String,
    pub base_gitlinks: BTreeMap<String, String>,
    pub disabled_filters: Vec<String>,
    #[serde(default)]
    pub lfs_enabled: bool,
}

pub struct GitWorkspace {
    main_git_dir: PathBuf,
    workspace_path: PathBuf,
    index_file: PathBuf,
    branch: String,
    base_commit: String,
    base_tree: String,
    base_gitlinks: BTreeMap<String, String>,
    disabled_filters: Vec<String>,
    lfs_enabled: bool,
}

pub const MAX_DIFF_BYTES: usize = 2 * 1024 * 1024;

/// Of the caller's `GIT_*` variables, only configuration location and identity are kept: the rest
/// (`GIT_DIR`, `GIT_EXTERNAL_DIFF`, ...) would change what the engine runs, or on which repository.
fn keeps_git_variable(name: &str) -> bool {
    matches!(
        name,
        "GIT_CONFIG_GLOBAL" | "GIT_CONFIG_SYSTEM" | "GIT_CONFIG_NOSYSTEM"
    ) || name.starts_with("GIT_AUTHOR_")
        || name.starts_with("GIT_COMMITTER_")
}

/// Base of every git command of the engine.
fn git() -> Command {
    let mut cmd = Command::new("git");
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("GIT_") && !keeps_git_variable(&name) {
            cmd.env_remove(name.as_ref());
        }
    }
    cmd.env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "core.trustctime=true",
        "core.checkStat=default",
        // Never enter, move or fetch a submodule, whatever the user's configuration says.
        "submodule.recurse=false",
    ] {
        cmd.args(["-c", setting]);
    }
    cmd
}

fn validate_object_id(oid: &str, label: &str) -> Result<()> {
    let len = oid.len();
    if (len != 40 && len != 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{label} inválido: {oid:?}");
    }
    Ok(())
}

fn run_git_checked(mut cmd: Command, context_msg: &str) -> Result<Output> {
    let out = cmd
        .output()
        .with_context(|| format!("falha ao executar git ({context_msg})"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("{context_msg}: {stderr}");
    }
    Ok(out)
}

/// Runs git with `input` on its stdin; the error names the step, prefixed by `context_msg`.
fn run_git_with_input(mut cmd: Command, input: Vec<u8>, context_msg: &str) -> Result<Vec<u8>> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("falha ao executar git ({context_msg})"))?;
    let mut stdin = child.stdin.take().context("stdin do git ausente")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child
        .wait_with_output()
        .with_context(|| format!("falha ao executar git ({context_msg})"))?;
    let _ = writer.join();
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if stderr.is_empty() {
            bail!("{context_msg}");
        }
        bail!("{context_msg}: {stderr}");
    }
    Ok(out.stdout)
}

fn repo_git_cmd(repo: &Path) -> Command {
    let mut cmd = git();
    cmd.arg("-C").arg(repo);
    cmd
}

fn main_git_dir_cmd(git_dir: &Path) -> Command {
    let mut cmd = git();
    cmd.env("GIT_DIR", git_dir);
    cmd
}

/// Names of every clean/smudge filter configured for the repository (any scope), with its
/// `(setting, value)` pairs in config order.
fn configured_filters(git_dir: &Path) -> Result<BTreeMap<String, Vec<(String, String)>>> {
    let mut cmd = main_git_dir_cmd(git_dir);
    cmd.args([
        "config",
        "-z",
        "--get-regexp",
        r"^filter\..*\.(clean|smudge|process)$",
    ]);
    let out = cmd.output().context("falha ao ler filtros do git")?;
    match out.status.code() {
        Some(0) => {}
        // No match.
        Some(1) => return Ok(BTreeMap::new()),
        _ => bail!(
            "falha ao ler filtros do git: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
    let mut filters: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8(entry.to_vec()).context("filtro git com nome inválido")?;
        let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
        let (name, setting) = key
            .strip_prefix("filter.")
            .and_then(|k| k.rsplit_once('.'))
            .with_context(|| format!("filtro git inesperado: {key:?}"))?;
        filters
            .entry(name.to_string())
            .or_default()
            .push((setting.to_string(), value.to_string()));
    }
    Ok(filters)
}

/// Filters that files of `repo_root` use through attributes.
fn filters_in_use(repo_root: &Path) -> Result<BTreeSet<String>> {
    let mut ls = repo_git_cmd(repo_root);
    ls.args(["ls-files", "-z"]);
    let files = run_git_checked(ls, "falha ao listar arquivos do repositório")?.stdout;
    let mut check = repo_git_cmd(repo_root);
    check.args(["check-attr", "-z", "--stdin", "filter"]);
    let out = run_git_with_input(check, files, "falha ao ler atributos do git")?;
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    Ok(fields
        .as_chunks::<3>()
        .0
        .iter()
        .map(|f| String::from_utf8_lossy(f[2]).into_owned())
        .filter(|v| !matches!(v.as_str(), "unspecified" | "unset" | "set"))
        .collect())
}

/// The commands `git lfs install` writes. Git runs them through a shell, so nothing may follow.
const GIT_LFS_COMMANDS: &[&str] = &[
    "git-lfs clean -- %f",
    "git-lfs clean %f",
    "git-lfs smudge -- %f",
    "git-lfs smudge %f",
    "git-lfs smudge --skip -- %f",
    "git-lfs smudge --skip %f",
    "git-lfs filter-process",
    "git-lfs filter-process --skip",
];

/// The filters the checkout disables, and the LFS commands it may run when the repository uses a
/// trusted LFS filter.
struct FilterPolicy {
    disabled: Vec<String>,
    lfs: Option<BTreeMap<String, String>>,
}

/// The commands the checkout may run for `lfs`: the last value per key, without `--skip` (the goal
/// is real content), and only when smudge or process exists, or no pointer would ever be read.
fn lfs_checkout_commands(settings: &[(String, String)]) -> Option<BTreeMap<String, String>> {
    let mut commands: BTreeMap<String, String> = BTreeMap::new();
    for (setting, value) in settings {
        commands.insert(setting.clone(), value.replace(" --skip", ""));
    }
    if commands.contains_key("smudge") || commands.contains_key("process") {
        Some(commands)
    } else {
        None
    }
}

/// A filter command runs outside the sandbox with the workspace as its directory, over files the
/// agents wrote. Only Git LFS is trusted, and only where the repository already uses it. Every
/// other filter is switched off; a repository that depends on one is refused, because storing its
/// files unfiltered would change (or, for git-crypt, disclose) their content.
fn filters_to_disable(git_dir: &Path, repo_root: &Path) -> Result<FilterPolicy> {
    let in_use = filters_in_use(repo_root)?;
    let mut policy = FilterPolicy {
        disabled: Vec::new(),
        lfs: None,
    };
    for (name, settings) in configured_filters(git_dir)? {
        let is_lfs = name == "lfs"
            && settings
                .iter()
                .all(|(_, c)| GIT_LFS_COMMANDS.contains(&c.as_str()));
        match (in_use.contains(&name), is_lfs) {
            (true, true) => {
                if name == "lfs" {
                    policy.lfs = lfs_checkout_commands(&settings);
                }
            }
            (true, false) => bail!(
                "o repositório usa o filtro git '{name}', que executaria fora do sandbox; não suportado pelo loop"
            ),
            (false, _) => policy.disabled.push(name),
        }
    }
    Ok(policy)
}

/// How the content of a blob of `base_commit` relates to Git LFS.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LfsBlob {
    Pointer { oid: String, size: u64 },
    NonCanonical,
    Content,
}

const LFS_POINTER_VERSION: &str = "version https://git-lfs.github.com/spec/v1";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The `(oid, size)` of a canonical pointer, rebuilt and compared: anything else (CRLF, extra
/// lines, a leading zero, size 0, another version) is not canonical.
fn canonical_pointer_fields(blob: &[u8]) -> Option<(String, u64)> {
    let text = std::str::from_utf8(blob).ok()?;
    let mut lines = text.strip_suffix('\n')?.split('\n');
    if lines.next()? != LFS_POINTER_VERSION {
        return None;
    }
    let oid = lines.next()?.strip_prefix("oid sha256:")?;
    if oid.len() != 64 || !oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let size = lines.next()?.strip_prefix("size ")?;
    if lines.next().is_some() || size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Smudge reads content from the object, never from this field, but 0 has no canonical form.
    if size == "0" || size.len() > 1 && size.starts_with('0') {
        return None;
    }
    let size: u64 = size.parse().ok()?;
    Some((oid.to_string(), size))
}

/// Whether `blob` is an LFS pointer, and whether it is the canonical form a checkout can
/// reproduce without changing the snapshot tree.
fn classify_lfs_blob(blob: &[u8]) -> LfsBlob {
    if let Some((oid, size)) = canonical_pointer_fields(blob) {
        return LfsBlob::Pointer { oid, size };
    }
    let looks_like_pointer = contains(blob, b"oid sha256:")
        && [b"git-lfs".as_slice(), b"git-media", b"hawser"]
            .iter()
            .any(|marker| contains(blob, marker));
    if looks_like_pointer {
        LfsBlob::NonCanonical
    } else {
        LfsBlob::Content
    }
}

/// The first paths of an error, bounded and with control characters escaped.
fn lfs_paths_summary(paths: &[Vec<u8>]) -> String {
    let shown: Vec<String> = paths
        .iter()
        .take(3)
        .map(|path| String::from_utf8_lossy(path).escape_debug().to_string())
        .collect();
    let mut summary = shown.join(", ");
    if paths.len() > 3 {
        summary.push_str(&format!(" e mais {}", paths.len() - 3));
    }
    summary
}

/// Whether `oid` is in the local store with exactly `size` bytes. Smudge deletes a wrong-size
/// object, so a truncated one counts as missing and is never passed to it.
fn lfs_object_present(storage: &Path, oid: &str, size: u64) -> Result<bool> {
    let path = storage
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    match fs::metadata(&path) {
        Ok(meta) => Ok(meta.is_file() && meta.len() == size),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e)
            .with_context(|| format!("falha ao consultar objeto do Git LFS {}", path.display())),
    }
}

/// Reads `cat-file --batch` output: `<oid> blob <size>\n<bytes>\n` for each requested object.
fn parse_batch_blobs(raw: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut blobs = BTreeMap::new();
    let mut rest = raw;
    while !rest.is_empty() {
        let Some(end) = rest.iter().position(|&b| b == b'\n') else {
            bail!("falha ao ler blobs do commit base: saída de cat-file truncada");
        };
        let header = std::str::from_utf8(&rest[..end])
            .context("falha ao ler blobs do commit base: cabeçalho inválido")?;
        rest = &rest[end + 1..];
        let mut fields = header.split(' ');
        let (Some(oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("falha ao ler blobs do commit base: resposta inesperada de cat-file");
        };
        let size: usize = size
            .parse()
            .context("falha ao ler blobs do commit base: tamanho inválido")?;
        if kind != "blob" {
            bail!("falha ao ler blobs do commit base: objeto {oid} não é um blob");
        }
        if rest.len() < size + 1 || rest[size] != b'\n' {
            bail!("falha ao ler blobs do commit base: conteúdo truncado");
        }
        blobs.insert(oid.to_string(), rest[..size].to_vec());
        rest = &rest[size + 1..];
    }
    Ok(blobs)
}

/// Refuses a run whose `base_commit` needs LFS content that the local store cannot provide, and
/// pointers that a smudge would rewrite. Runs no `git-lfs` and writes nothing.
fn check_lfs_objects(repo_root: &Path, base_commit: &str, storage: &Path) -> Result<()> {
    let mut ls = repo_git_cmd(repo_root);
    ls.args(["ls-tree", "-r", "-l", "-z", base_commit]);
    let listing = run_git_checked(ls, "falha ao listar a árvore do commit base")?;

    // A pointer is far smaller than 1024 bytes; anything bigger is content.
    let mut candidates: Vec<(Vec<u8>, String)> = Vec::new();
    for entry in listing.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let fields: Vec<&[u8]> = entry[..tab]
            .split(|b| b.is_ascii_whitespace())
            .filter(|f| !f.is_empty())
            .collect();
        let [mode, kind, oid, size] = fields.as_slice() else {
            continue;
        };
        if *mode != b"100644" && *mode != b"100755" || *kind != b"blob" {
            continue;
        }
        let Ok(size) = std::str::from_utf8(size).unwrap_or("").parse::<u64>() else {
            continue;
        };
        if size >= 1024 {
            continue;
        }
        candidates.push((
            entry[tab + 1..].to_vec(),
            String::from_utf8_lossy(oid).into_owned(),
        ));
    }
    if candidates.is_empty() {
        return Ok(());
    }

    let mut input = Vec::new();
    for (path, _) in &candidates {
        input.extend_from_slice(path);
        input.push(0);
    }
    let mut check = repo_git_cmd(repo_root);
    check.args([
        "check-attr",
        "-z",
        "--stdin",
        &format!("--source={base_commit}"),
        "filter",
    ]);
    let attrs = run_git_with_input(
        check,
        input,
        "falha ao ler atributos do commit base (requer git 2.40 ou mais novo)",
    )?;
    let fields: Vec<&[u8]> = attrs.split(|&b| b == 0).collect();
    let lfs_paths: BTreeSet<&[u8]> = fields
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|chunk| chunk[2] == b"lfs")
        .map(|chunk| chunk[0])
        .collect();

    // Unique blobs, in ls-tree order, so a pointer shared by two paths is read once.
    let mut blob_oids: Vec<&str> = Vec::new();
    let mut lfs_files: Vec<(&[u8], &str)> = Vec::new();
    for (path, oid) in &candidates {
        if !lfs_paths.contains(path.as_slice()) {
            continue;
        }
        if !blob_oids.contains(&oid.as_str()) {
            blob_oids.push(oid);
        }
        lfs_files.push((path, oid));
    }
    if lfs_files.is_empty() {
        return Ok(());
    }

    let mut input = Vec::new();
    for oid in &blob_oids {
        input.extend_from_slice(oid.as_bytes());
        input.push(b'\n');
    }
    let mut cat = repo_git_cmd(repo_root);
    cat.args(["cat-file", "--batch"]);
    let blobs = parse_batch_blobs(&run_git_with_input(
        cat,
        input,
        "falha ao ler blobs do commit base",
    )?)?;

    let mut non_canonical: Vec<Vec<u8>> = Vec::new();
    let mut seen_non_canonical: BTreeSet<&str> = BTreeSet::new();
    let mut missing: Vec<Vec<u8>> = Vec::new();
    let mut seen_missing: BTreeSet<String> = BTreeSet::new();
    let mut presence: BTreeMap<String, bool> = BTreeMap::new();
    for (path, blob_oid) in &lfs_files {
        let blob = blobs
            .get(*blob_oid)
            .with_context(|| format!("falha ao ler blobs do commit base: {blob_oid} ausente"))?;
        match classify_lfs_blob(blob) {
            LfsBlob::Pointer { oid, size } => {
                if seen_missing.contains(&oid) {
                    continue;
                }
                let present = match presence.get(&oid) {
                    Some(present) => *present,
                    None => {
                        let present = lfs_object_present(storage, &oid, size)?;
                        presence.insert(oid.clone(), present);
                        present
                    }
                };
                if !present {
                    seen_missing.insert(oid);
                    missing.push(path.to_vec());
                }
            }
            LfsBlob::NonCanonical => {
                if seen_non_canonical.insert(blob_oid) {
                    non_canonical.push(path.to_vec());
                }
            }
            LfsBlob::Content => {}
        }
    }

    if !non_canonical.is_empty() {
        bail!(
            "{} ponteiro(s) do Git LFS fora do formato canônico: {}; não suportado pelo loop",
            non_canonical.len(),
            lfs_paths_summary(&non_canonical)
        );
    }
    if !missing.is_empty() {
        bail!(
            "faltam {} objeto(s) do Git LFS no repositório: {}; rode 'git lfs fetch' e tente de novo",
            missing.len(),
            lfs_paths_summary(&missing)
        );
    }
    Ok(())
}

/// The local LFS store of the repository: `lfs.storage` when absolute, joined to the common git
/// dir (a linked worktree follows `commondir`) when relative, `.git/lfs` by default. `.lfsconfig`
/// cannot set it; git-lfs only reads it from git config.
fn lfs_storage(git_dir: &Path) -> Result<PathBuf> {
    let mut cmd = main_git_dir_cmd(git_dir);
    cmd.args(["config", "-z", "--get", "lfs.storage"]);
    let out = cmd.output().context("falha ao ler lfs.storage do git")?;
    let value = match out.status.code() {
        Some(0) => String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\0')
            .to_string(),
        // Not configured.
        Some(1) => String::new(),
        _ => bail!(
            "falha ao ler lfs.storage do git: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    };
    let value = value.trim();
    // Default (`lfs`), or a relative value, is relative to the common git dir: a linked worktree
    // uses the main repository's store.
    let storage = if value.is_empty() {
        PathBuf::from("lfs")
    } else {
        PathBuf::from(value)
    };
    if storage.is_absolute() {
        return Ok(storage);
    }
    let mut common_cmd = main_git_dir_cmd(git_dir);
    common_cmd.args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let common_out = run_git_checked(common_cmd, "falha ao localizar .git comum")?;
    let common = PathBuf::from(String::from_utf8_lossy(&common_out.stdout).trim());
    Ok(common.join(storage))
}

/// The text of a gitlink line of `ls-tree`/`ls-files --stage`; `None` for any other entry. A path
/// that is not UTF-8 is refused: skipping it would leave that submodule unchecked.
fn gitlink_entry(entry: &[u8]) -> Result<Option<&str>> {
    if !entry.starts_with(b"160000 ") {
        return Ok(None);
    }
    let text = std::str::from_utf8(entry).context("submódulo com caminho que não é UTF-8")?;
    Ok(Some(text))
}

fn parse_gitlinks_from_ls_tree(raw: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for entry in raw.split(|&b| b == 0) {
        // Format: "<mode> <type> <oid>\t<path>"
        let Some((meta, path)) = gitlink_entry(entry)?.and_then(|s| s.split_once('\t')) else {
            continue;
        };
        if let Some(oid) = meta.split_whitespace().nth(2) {
            map.insert(path.to_string(), oid.to_string());
        }
    }
    Ok(map)
}

fn parse_gitlinks_from_ls_files_stage(raw: &[u8]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in raw.split(|&b| b == 0) {
        // Format: "<mode> <oid> <stage>\t<path>"
        let Some((meta, path)) = gitlink_entry(entry)?.and_then(|s| s.split_once('\t')) else {
            continue;
        };
        if let Some(oid) = meta.split_whitespace().nth(1) {
            out.push((path.to_string(), oid.to_string()));
        }
    }
    Ok(out)
}

/// The settings that turn every untrusted filter of the repository off, in this order.
fn disabled_filter_config(filters: &[String]) -> Vec<(String, String)> {
    let mut config = Vec::new();
    for name in filters {
        for (key, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            config.push((format!("filter.{name}.{key}"), value.to_string()));
        }
    }
    config
}

/// The settings of the checkout of a repository with the trusted `lfs` filter.
fn lfs_checkout_config(lfs: &LfsCheckout) -> Vec<(String, String)> {
    let mut config: Vec<(String, String)> = lfs
        .commands
        .iter()
        .map(|(key, value)| (format!("filter.lfs.{key}"), value.clone()))
        .collect();
    config.push(("filter.lfs.required".to_string(), "true".to_string()));
    config.push(("lfs.storage".to_string(), lfs.storage.display().to_string()));
    // The three below outrank the repository's `.lfsconfig`, which could otherwise choose an
    // address (`lfs.url`) or leave pointers in place (`fetchinclude`/`fetchexclude`).
    config.push(("lfs.url".to_string(), "file:///dev/null".to_string()));
    config.push(("lfs.fetchinclude".to_string(), String::new()));
    config.push(("lfs.fetchexclude".to_string(), String::new()));
    config
}

/// Through `GIT_CONFIG_*` and not `-c`: a filter name may contain `=`.
fn set_git_config(cmd: &mut Command, config: &[(String, String)]) {
    for (slot, (key, value)) in config.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{slot}"), key)
            .env(format!("GIT_CONFIG_VALUE_{slot}"), value);
    }
    cmd.env("GIT_CONFIG_COUNT", config.len().to_string());
}

fn set_disabled_filters(cmd: &mut Command, filters: &[String]) {
    set_git_config(cmd, &disabled_filter_config(filters));
}

impl GitWorkspace {
    /// Read-only validation of the repository and branch target. Creates no directories or refs.
    pub fn preflight_check_repo(
        repo: &Path,
        branch: &str,
        from_head: bool,
    ) -> Result<RepoPreflightInfo> {
        if !repo.is_dir() {
            bail!(
                "caminho do repositório não é um diretório: {}",
                repo.display()
            );
        }
        if branch.is_empty() || branch.starts_with('-') {
            bail!("nome de branch inválido: {branch:?}");
        }

        let mut check_branch = git();
        check_branch.args(["check-ref-format", "--branch", branch]);
        run_git_checked(check_branch, &format!("nome de branch inválido '{branch}'"))?;

        let mut toplevel_cmd = repo_git_cmd(repo);
        toplevel_cmd.args(["rev-parse", "--show-toplevel"]);
        let toplevel_out = run_git_checked(toplevel_cmd, "não é um repositório git válido")?;
        let repo_root = PathBuf::from(String::from_utf8_lossy(&toplevel_out.stdout).trim());

        let mut gitdir_cmd = repo_git_cmd(&repo_root);
        gitdir_cmd.args(["rev-parse", "--absolute-git-dir"]);
        let gitdir_out = run_git_checked(gitdir_cmd, "falha ao localizar .git principal")?;
        let main_git_dir = PathBuf::from(String::from_utf8_lossy(&gitdir_out.stdout).trim());

        if fs::symlink_metadata(repo_root.join(".tchelvis-loop")).is_ok() {
            bail!("o repositório já possui '.tchelvis-loop'; remova antes de iniciar o loop");
        }

        let mut head_cmd = repo_git_cmd(&repo_root);
        head_cmd.args(["rev-parse", "--verify", "HEAD"]);
        let head_out = head_cmd
            .output()
            .context("falha ao verificar HEAD do repositório")?;
        if !head_out.status.success() {
            bail!("repositório sem commits (HEAD ausente)");
        }
        let base_commit = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
        validate_object_id(&base_commit, "base_commit")?;

        let mut tree_cmd = main_git_dir_cmd(&main_git_dir);
        tree_cmd.args(["rev-parse", &format!("{base_commit}^{{tree}}")]);
        let tree_out = run_git_checked(tree_cmd, "falha ao obter árvore do commit base")?;
        let base_tree = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();
        validate_object_id(&base_tree, "base_tree")?;

        let mut ref_cmd = main_git_dir_cmd(&main_git_dir);
        ref_cmd.args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ]);
        let ref_status = ref_cmd
            .status()
            .context("falha ao verificar existência da branch")?;
        if ref_status.success() {
            bail!("a branch '{branch}' já existe no repositório");
        }

        // Before `status`, which runs the filters.
        let policy = filters_to_disable(&main_git_dir, &repo_root)?;
        let disabled_filters = policy.disabled;
        let lfs = match policy.lfs {
            Some(commands) => Some(LfsCheckout {
                commands,
                storage: lfs_storage(&main_git_dir)?,
            }),
            None => None,
        };

        if !from_head {
            let mut status_cmd = repo_git_cmd(&repo_root);
            set_disabled_filters(&mut status_cmd, &disabled_filters);
            // `.loops/` holds the workflows themselves, usually not committed yet. The content of
            // a submodule never reaches the workspace, and looking at it would run its
            // configuration: only its commit counts.
            status_cmd.args([
                "status",
                "--porcelain",
                "--ignore-submodules=dirty",
                "--",
                ":/",
                ":(top,exclude).loops",
            ]);
            let status_out =
                run_git_checked(status_cmd, "falha ao verificar status do repositório")?;
            if !String::from_utf8_lossy(&status_out.stdout)
                .trim()
                .is_empty()
            {
                bail!("repositório possui mudanças não commitadas (use --from-head para partir de HEAD)");
            }
        }

        if let Some(lfs) = &lfs {
            check_lfs_objects(&repo_root, &base_commit, &lfs.storage)?;
        }

        Ok(RepoPreflightInfo {
            repo_root,
            main_git_dir,
            branch: branch.to_string(),
            base_commit,
            base_tree,
            disabled_filters,
            lfs,
        })
    }

    pub fn create_in(
        info: RepoPreflightInfo,
        run_dir: &Path,
        workspace_path: &Path,
    ) -> Result<Self> {
        fs::create_dir_all(run_dir)
            .with_context(|| format!("falha ao criar diretório da run {}", run_dir.display()))?;
        if let Some(ws_parent) = workspace_path.parent() {
            fs::create_dir_all(ws_parent).with_context(|| {
                format!(
                    "falha ao criar diretório pai do workspace {}",
                    ws_parent.display()
                )
            })?;
        }
        if fs::symlink_metadata(workspace_path).is_ok() {
            bail!(
                "workspace já existe em {}; use outro run_id",
                workspace_path.display()
            );
        }
        let index_file = run_dir.join("engine.index");
        let lfs_enabled = info.lfs.is_some();

        let mut clone_cmd = git();
        set_disabled_filters(&mut clone_cmd, &info.disabled_filters);
        clone_cmd
            .args([
                "clone",
                "-q",
                "--no-hardlinks",
                "--template=",
                "--no-checkout",
            ])
            .arg(&info.repo_root)
            .arg(workspace_path);
        run_git_checked(clone_cmd, "falha ao clonar repositório para o workspace")?;

        // The only commands that use the workspace's own .git, before any agent runs.
        let own_git = || {
            let mut cmd = git();
            set_disabled_filters(&mut cmd, &info.disabled_filters);
            cmd.env("GIT_DIR", workspace_path.join(".git"))
                .env("GIT_WORK_TREE", workspace_path);
            cmd
        };
        // Only the checkout materialises files: it carries the disabled filters and, when the
        // repository uses LFS, the trusted commands and the local store — in one block, because a
        // second `set_git_config` call would restart at slot 0.
        let mut co_config = disabled_filter_config(&info.disabled_filters);
        if let Some(lfs) = &info.lfs {
            co_config.extend(lfs_checkout_config(lfs));
        }
        let mut co_cmd = git();
        set_git_config(&mut co_cmd, &co_config);
        co_cmd
            .env("GIT_DIR", workspace_path.join(".git"))
            .env("GIT_WORK_TREE", workspace_path)
            .args(["checkout", "-q", "--detach", &info.base_commit]);
        run_git_checked(
            co_cmd,
            "falha ao fazer checkout de base_commit no workspace",
        )?;
        let mut remote_cmd = own_git();
        remote_cmd.args(["remote", "remove", "origin"]);
        run_git_checked(remote_cmd, "falha ao remover remote origin do workspace")?;

        let mut ls_tree_cmd = main_git_dir_cmd(&info.main_git_dir);
        ls_tree_cmd.args(["ls-tree", "-r", "-z", &info.base_tree]);
        let ls_tree_out = run_git_checked(ls_tree_cmd, "falha ao inspecionar árvore base")?;
        let base_gitlinks = parse_gitlinks_from_ls_tree(&ls_tree_out.stdout)?;

        let ws = Self {
            main_git_dir: info.main_git_dir,
            workspace_path: workspace_path.to_path_buf(),
            index_file,
            branch: info.branch,
            base_commit: info.base_commit,
            base_tree: info.base_tree,
            base_gitlinks,
            disabled_filters: info.disabled_filters,
            lfs_enabled,
        };

        let mut init_idx = ws.engine_git();
        init_idx.args(["read-tree", &ws.base_tree]);
        run_git_checked(init_idx, "falha ao inicializar índice da run")?;

        Ok(ws)
    }

    pub fn create(repo: &Path, run_dir: &Path, branch: &str, from_head: bool) -> Result<Self> {
        let info = Self::preflight_check_repo(repo, branch, from_head)?;
        Self::create_in(info, run_dir, &run_dir.join("workspace"))
    }

    pub fn to_frozen(&self) -> FrozenWorkspaceMeta {
        FrozenWorkspaceMeta {
            main_git_dir: self.main_git_dir.clone(),
            workspace_path: self.workspace_path.clone(),
            index_file: self.index_file.clone(),
            branch: self.branch.clone(),
            base_commit: self.base_commit.clone(),
            base_tree: self.base_tree.clone(),
            base_gitlinks: self.base_gitlinks.clone(),
            disabled_filters: self.disabled_filters.clone(),
            lfs_enabled: self.lfs_enabled,
        }
    }

    pub fn from_frozen(meta: FrozenWorkspaceMeta) -> Self {
        Self {
            main_git_dir: meta.main_git_dir,
            workspace_path: meta.workspace_path,
            index_file: meta.index_file,
            branch: meta.branch,
            base_commit: meta.base_commit,
            base_tree: meta.base_tree,
            base_gitlinks: meta.base_gitlinks,
            disabled_filters: meta.disabled_filters,
            lfs_enabled: meta.lfs_enabled,
        }
    }

    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub fn base_tree(&self) -> &str {
        &self.base_tree
    }

    /// Always uses the main repository's GIT_DIR, the workspace's GIT_WORK_TREE,
    /// and a run-private GIT_INDEX_FILE with hooks and fsmonitor disabled.
    fn engine_git(&self) -> Command {
        let mut cmd = git();
        set_disabled_filters(&mut cmd, &self.disabled_filters);
        cmd.args(["-c", "core.sparseCheckout=false"])
            .env("GIT_DIR", &self.main_git_dir)
            .env("GIT_WORK_TREE", &self.workspace_path)
            .env("GIT_INDEX_FILE", &self.index_file)
            .current_dir(&self.workspace_path);
        cmd
    }

    /// Submodules are never checked out, so their directories stay empty. Git would run commands
    /// inside a repository planted there, with the agent's configuration and outside the sandbox.
    fn check_submodule_dirs_untouched(&self) -> Result<()> {
        for path in self.base_gitlinks.keys() {
            let full = self.workspace_path.join(path);
            let empty = match fs::symlink_metadata(&full) {
                Ok(meta) if meta.is_dir() => fs::read_dir(&full)
                    .with_context(|| format!("falha ao ler o submódulo '{path}'"))?
                    .next()
                    .is_none(),
                Ok(_) => true,
                Err(e) => matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ),
            };
            if !empty {
                bail!("submódulo alterado em '{path}': a pasta de um submódulo deve ficar vazia no workspace do loop");
            }
        }
        Ok(())
    }

    fn git_config_get(&self, key: &str) -> Option<String> {
        let mut cmd = main_git_dir_cmd(&self.main_git_dir);
        cmd.args(["config", "--get", key]);
        let out = cmd.output().ok()?;
        if out.status.success() {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
        None
    }
}

impl Workspace for GitWorkspace {
    fn snapshot(&mut self) -> Result<String> {
        self.check_submodule_dirs_untouched()?;
        // Entries read from a tree carry no stat data, so every file is read again: forged size and
        // times (git compares whole seconds) cannot hide a change.
        let mut reset_cmd = self.engine_git();
        reset_cmd.args(["read-tree", &self.base_tree]);
        run_git_checked(reset_cmd, "falha ao reiniciar o índice da run")?;

        let mut add_cmd = self.engine_git();
        add_cmd.args(["add", "-A", "--", ".", ":!.tchelvis-loop"]);
        run_git_checked(add_cmd, "falha no snapshot (git add)")?;

        let mut stage_cmd = self.engine_git();
        stage_cmd.args(["ls-files", "--stage", "-z"]);
        let stage_out = run_git_checked(stage_cmd, "falha ao inspecionar índice após add")?;
        for (path, oid) in parse_gitlinks_from_ls_files_stage(&stage_out.stdout)? {
            match self.base_gitlinks.get(&path) {
                Some(base_oid) if base_oid == &oid => {}
                Some(_) => bail!("submódulo alterado em '{path}'"),
                None => bail!("repositório aninhado detectado em '{path}'"),
            }
        }

        let mut write_cmd = self.engine_git();
        write_cmd.arg("write-tree");
        let write_out = run_git_checked(write_cmd, "falha ao gravar tree no snapshot")?;
        let tree = String::from_utf8_lossy(&write_out.stdout)
            .trim()
            .to_string();
        validate_object_id(&tree, "snapshot tree")?;
        Ok(tree)
    }

    fn restore(&mut self, tree: &str) -> Result<()> {
        validate_object_id(tree, "restore tree")?;
        self.check_submodule_dirs_untouched()?;

        let mut read_cmd = self.engine_git();
        read_cmd.args(["read-tree", "--reset", "-u", tree]);
        run_git_checked(read_cmd, "falha ao restaurar árvore do workspace")?;

        let mut clean_cmd = self.engine_git();
        clean_cmd.args(["clean", "-ffd", "-e", ".git", "-e", ".tchelvis-loop"]);
        run_git_checked(
            clean_cmd,
            "falha ao limpar arquivos adicionados no workspace",
        )?;

        Ok(())
    }

    fn diff(&self, tree: &str) -> Result<String> {
        validate_object_id(tree, "diff tree")?;
        let mut cmd = main_git_dir_cmd(&self.main_git_dir);
        cmd.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--submodule=short",
            &self.base_tree,
            tree,
        ]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn().context("falha ao gerar diff da run")?;
        let stdout = child.stdout.take().context("stdout do diff ausente")?;
        let mut bytes = Vec::new();
        stdout
            .take(MAX_DIFF_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .context("falha ao ler diff da run")?;
        if bytes.len() > MAX_DIFF_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            bytes.truncate(MAX_DIFF_BYTES);
            let mut text = String::from_utf8_lossy(&bytes).into_owned();
            text.truncate(text.rfind('\n').map_or(0, |i| i + 1));
            text.push_str("[tchelvis: diff truncado]\n");
            return Ok(text);
        }
        if !child
            .wait()
            .context("falha ao gerar diff da run")?
            .success()
        {
            bail!("falha ao gerar diff da run");
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn changed_files(&self, tree: &str) -> Result<Vec<String>> {
        validate_object_id(tree, "changed_files tree")?;
        let mut cmd = main_git_dir_cmd(&self.main_git_dir);
        cmd.args([
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--submodule=short",
            &self.base_tree,
            tree,
        ]);
        let out = run_git_checked(cmd, "falha ao listar arquivos alterados")?;
        let files = out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        Ok(files)
    }

    fn publish(&mut self, tree: &str, message: &str) -> Result<()> {
        validate_object_id(tree, "publish tree")?;

        let full_ref = format!("refs/heads/{}", self.branch);
        let mut check_ref = main_git_dir_cmd(&self.main_git_dir);
        check_ref.args(["show-ref", "--verify", "--quiet", &full_ref]);
        let status = check_ref
            .status()
            .context("falha ao verificar branch antes de publicar")?;
        if status.success() {
            bail!("a branch '{}' já existe no repositório", self.branch);
        }

        let mut commit_cmd = main_git_dir_cmd(&self.main_git_dir);
        if self.git_config_get("user.name").is_none()
            && std::env::var_os("GIT_AUTHOR_NAME").is_none()
        {
            commit_cmd.env("GIT_AUTHOR_NAME", "tchelvis");
            commit_cmd.env("GIT_COMMITTER_NAME", "tchelvis");
        }
        if self.git_config_get("user.email").is_none()
            && std::env::var_os("GIT_AUTHOR_EMAIL").is_none()
        {
            commit_cmd.env("GIT_AUTHOR_EMAIL", "tchelvis@localhost");
            commit_cmd.env("GIT_COMMITTER_EMAIL", "tchelvis@localhost");
        }
        commit_cmd.args(["commit-tree", tree, "-p", &self.base_commit, "-m", message]);
        let commit_out = run_git_checked(commit_cmd, "falha ao criar commit da branch")?;
        let commit_sha = String::from_utf8_lossy(&commit_out.stdout)
            .trim()
            .to_string();
        validate_object_id(&commit_sha, "commit_sha")?;

        let zero_oid = "0".repeat(commit_sha.len());
        let mut update_cmd = main_git_dir_cmd(&self.main_git_dir);
        update_cmd.args(["update-ref", &full_ref, &commit_sha, &zero_oid]);
        run_git_checked(
            update_cmd,
            &format!("falha ao criar referência '{}'", self.branch),
        )?;
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.workspace_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_cmd(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("spawn git")
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = git_cmd(dir, args);
        assert!(
            out.status.success(),
            "git {:?} failed in {}: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo_with_commit(dir: &Path) {
        git_ok(dir, &["init", "-b", "main"]);
        git_ok(dir, &["config", "user.name", "Test User"]);
        git_ok(dir, &["config", "user.email", "test@example.com"]);
        fs::write(dir.join("README.md"), "hello\n").unwrap();
        fs::write(dir.join("keep.txt"), "keep\n").unwrap();
        fs::write(dir.join(".gitignore"), "*.log\nignored_dir/\n").unwrap();
        git_ok(dir, &["add", "."]);
        git_ok(dir, &["commit", "-m", "initial"]);
    }

    #[test]
    fn hostile_workspace_runs_nothing() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);

        let mut ws = GitWorkspace::create(&repo, &run_dir, "loop/test-hostile", false).unwrap();

        let marker = tmp.path().join("pwned.marker");
        let marker_str = marker.to_str().unwrap();

        // Plant hostile git hooks, fsmonitor, diff.external, filter.x.clean inside workspace .git
        let ws_git = ws.path().join(".git");
        let hooks_dir = ws_git.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        for hook in ["pre-commit", "post-index-change", "post-checkout"] {
            let p = hooks_dir.join(hook);
            crate::test_support::write_executable(
                &p,
                format!("#!/usr/bin/env sh\ntouch '{marker_str}'\n"),
            );
        }
        let evil_hooks = ws.path().join("evil-hooks");
        fs::create_dir_all(&evil_hooks).unwrap();
        for hook in ["pre-commit", "post-index-change"] {
            crate::test_support::write_executable(
                evil_hooks.join(hook),
                format!("#!/usr/bin/env sh\ntouch '{marker_str}'\n"),
            );
        }

        let hostile_config = format!(
            "[core]\n\tfsmonitor = touch '{marker_str}'\n\thooksPath = {}\n[diff]\n\texternal = touch '{marker_str}'\n[filter \"x\"]\n\tclean = touch '{marker_str}'\n",
            evil_hooks.display()
        );
        let existing_cfg = fs::read_to_string(ws_git.join("config")).unwrap_or_default();
        fs::write(
            ws_git.join("config"),
            format!("{existing_cfg}\n{hostile_config}"),
        )
        .unwrap();

        fs::write(ws.path().join(".gitattributes"), "*.txt filter=x\n").unwrap();
        fs::write(ws.path().join("keep.txt"), "modified\n").unwrap();
        fs::write(ws.path().join("new.txt"), "brand new\n").unwrap();

        let tree = ws.snapshot().unwrap();
        let _diff = ws.diff(&tree).unwrap();
        let _files = ws.changed_files(&tree).unwrap();
        ws.restore(&tree).unwrap();
        ws.publish(&tree, "publish hostile test").unwrap();

        assert!(
            !marker.exists(),
            "hostile workspace hook/config executed and created marker!"
        );
    }

    #[test]
    fn snapshot_sees_new_files_and_agent_commits_but_not_ignored_or_exchange_files() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);

        let mut ws = GitWorkspace::create(&repo, &run_dir, "loop/test-snap", false).unwrap();

        // Agent creates ignored file, exchange dir, untracked file, and commits another file inside workspace
        fs::write(ws.path().join("debug.log"), "ignored\n").unwrap();
        fs::create_dir_all(ws.path().join("ignored_dir")).unwrap();
        fs::write(ws.path().join("ignored_dir/a.txt"), "ignored\n").unwrap();
        fs::create_dir_all(ws.path().join(".tchelvis-loop/out")).unwrap();
        fs::write(ws.path().join(".tchelvis-loop/out/review.md"), "exchange\n").unwrap();

        fs::write(ws.path().join("committed_by_agent.rs"), "fn a() {}\n").unwrap();
        git_ok(ws.path(), &["config", "user.name", "Agent"]);
        git_ok(ws.path(), &["config", "user.email", "agent@example.com"]);
        git_ok(ws.path(), &["add", "committed_by_agent.rs"]);
        git_ok(ws.path(), &["commit", "-m", "agent commit"]);

        fs::write(ws.path().join("untracked_by_agent.rs"), "fn b() {}\n").unwrap();

        let tree = ws.snapshot().unwrap();
        let files = ws.changed_files(&tree).unwrap();
        assert!(files.contains(&"committed_by_agent.rs".to_string()));
        assert!(files.contains(&"untracked_by_agent.rs".to_string()));
        assert!(!files.iter().any(|f| f.contains("debug.log")));
        assert!(!files.iter().any(|f| f.contains("ignored_dir")));
        assert!(!files.iter().any(|f| f.contains(".tchelvis-loop")));

        let diff = ws.diff(&tree).unwrap();
        assert!(diff.contains("committed_by_agent.rs"));
        assert!(diff.contains("untracked_by_agent.rs"));
        assert!(!diff.contains("debug.log"));
        assert!(!diff.contains(".tchelvis-loop"));
    }

    #[test]
    fn restore_undoes_edits_additions_and_deletions() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);

        let mut ws = GitWorkspace::create(&repo, &run_dir, "loop/test-restore", false).unwrap();
        let base_tree = ws.snapshot().unwrap();

        // Edit, add, delete
        fs::write(ws.path().join("README.md"), "corrupted\n").unwrap();
        fs::write(ws.path().join("added.txt"), "extra\n").unwrap();
        fs::create_dir_all(ws.path().join("added_dir")).unwrap();
        fs::write(ws.path().join("added_dir/nested.txt"), "nested\n").unwrap();
        fs::remove_file(ws.path().join("keep.txt")).unwrap();

        let dirty_tree = ws.snapshot().unwrap();
        assert_ne!(dirty_tree, base_tree);

        ws.restore(&base_tree).unwrap();
        assert_eq!(
            fs::read_to_string(ws.path().join("README.md")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            fs::read_to_string(ws.path().join("keep.txt")).unwrap(),
            "keep\n"
        );
        assert!(!ws.path().join("added.txt").exists());
        assert!(!ws.path().join("added_dir").exists());
        assert_eq!(ws.snapshot().unwrap(), base_tree);
    }

    #[test]
    fn publish_creates_the_branch_and_leaves_checkout_index_and_head_untouched() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);

        let head_before = git_ok(&repo, &["rev-parse", "HEAD"]);
        let branch_before = git_ok(&repo, &["symbolic-ref", "--short", "HEAD"]);

        let mut ws = GitWorkspace::create(&repo, &run_dir, "loop/test-publish", false).unwrap();
        // Also ensure origin remote was removed from workspace
        let remotes = git_ok(ws.path(), &["remote"]);
        assert!(
            remotes.is_empty(),
            "expected no remotes in workspace, got: {remotes}"
        );

        fs::write(ws.path().join("feature.txt"), "done\n").unwrap();
        let tree = ws.snapshot().unwrap();
        ws.publish(&tree, "loop result").unwrap();

        assert_eq!(git_ok(&repo, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            git_ok(&repo, &["symbolic-ref", "--short", "HEAD"]),
            branch_before
        );
        assert!(git_ok(&repo, &["status", "--porcelain"]).is_empty());
        assert!(!repo.join("feature.txt").exists());

        let branch_files = git_ok(&repo, &["ls-tree", "--name-only", "loop/test-publish"]);
        assert!(branch_files.lines().any(|l| l == "feature.txt"));
    }

    #[test]
    fn existing_branch_is_refused() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);
        git_ok(&repo, &["branch", "loop/existing"]);

        let err = GitWorkspace::create(&repo, &run_dir, "loop/existing", false)
            .err()
            .expect("expected existing branch to be refused");
        assert!(
            err.to_string().contains("já existe"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn dirty_repository_is_refused_unless_from_head() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        init_repo_with_commit(&repo);

        fs::write(repo.join("README.md"), "dirty uncommitted\n").unwrap();
        fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();

        let run1 = tmp.path().join("run1");
        fs::create_dir_all(&run1).unwrap();
        let err = GitWorkspace::create(&repo, &run1, "loop/dirty-no", false)
            .err()
            .expect("expected dirty repo to fail");
        assert!(
            err.to_string().contains("não commitadas"),
            "unexpected error: {err}"
        );

        let run2 = tmp.path().join("run2");
        fs::create_dir_all(&run2).unwrap();
        let ws = GitWorkspace::create(&repo, &run2, "loop/dirty-yes", true).unwrap();
        assert_eq!(
            fs::read_to_string(ws.path().join("README.md")).unwrap(),
            "hello\n"
        );
        assert!(!ws.path().join("untracked.txt").exists());
    }

    #[test]
    fn nested_repository_is_an_error_naming_the_path() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);

        let mut ws = GitWorkspace::create(&repo, &run_dir, "loop/nested", false).unwrap();
        let nested = ws.path().join("vendor/subrepo");
        fs::create_dir_all(&nested).unwrap();
        git_ok(&nested, &["init"]);
        fs::write(nested.join("a.txt"), "nested\n").unwrap();

        let err = ws.snapshot().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("vendor/subrepo"),
            "expected nested repo path in error, got: {msg}"
        );
    }

    #[test]
    fn repository_with_tchelvis_loop_dir_is_refused() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        init_repo_with_commit(&repo);
        fs::create_dir_all(repo.join(".tchelvis-loop")).unwrap();

        let err = GitWorkspace::create(&repo, &run_dir, "loop/has-exchange", false)
            .err()
            .expect("expected repo with .tchelvis-loop to be refused");
        assert!(err.to_string().contains(".tchelvis-loop"));
    }

    #[test]
    fn repository_without_commits_is_a_clear_error() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let run_dir = tmp.path().join("run");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        git_ok(&repo, &["init", "-b", "main"]);

        let err = GitWorkspace::create(&repo, &run_dir, "loop/empty", false)
            .err()
            .expect("expected empty repo to fail");
        let msg = err.to_string();
        assert!(
            msg.contains("sem commits") || msg.contains("HEAD"),
            "expected clear error, got: {msg}"
        );
    }

    fn new_repo(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let (repo, run) = (tmp.path().join("repo"), tmp.path().join("run"));
        fs::create_dir_all(&repo).unwrap();
        init_repo_with_commit(&repo);
        (repo, run)
    }

    #[test]
    fn a_filter_of_the_main_repository_never_runs_agent_files() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        let marker = tmp.path().join("pwned.marker");
        // A legitimate, relative filter command: with the workspace as cwd it would be the agent's.
        git_ok(&repo, &["config", "filter.x.clean", "./clean.sh"]);
        git_ok(&repo, &["config", "filter.x.smudge", "./clean.sh"]);
        git_ok(&repo, &["config", "filter.x.required", "true"]);
        let mut ws = GitWorkspace::create(&repo, &run, "loop/f", false).unwrap();
        let script = format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display());
        crate::test_support::write_executable(ws.path().join("clean.sh"), script);
        fs::write(ws.path().join(".gitattributes"), "* filter=x\n").unwrap();
        fs::write(ws.path().join("keep.txt"), "changed\n").unwrap();
        let tree = ws.snapshot().unwrap();
        ws.restore(&tree).unwrap();
        assert!(!marker.exists());
        assert!(ws.diff(&tree).unwrap().contains("+changed"));
    }

    #[test]
    fn a_repository_that_uses_an_unknown_filter_is_refused() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        fs::write(repo.join(".gitattributes"), "keep.txt filter=crypt\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "attr"]);
        git_ok(&repo, &["config", "filter.crypt.clean", "cat"]);
        let err = GitWorkspace::create(&repo, &run, "loop/f", false)
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("crypt"), "{err}");
    }

    #[test]
    fn git_variables_of_the_caller_are_not_inherited() {
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_EXTERNAL_DIFF",
            "GIT_EXEC_PATH",
            "GIT_TEMPLATE_DIR",
            "GIT_SSH_COMMAND",
        ] {
            assert!(!keeps_git_variable(name), "{name}");
        }
        for name in [
            "GIT_CONFIG_GLOBAL",
            "GIT_AUTHOR_NAME",
            "GIT_COMMITTER_EMAIL",
        ] {
            assert!(keeps_git_variable(name), "{name}");
        }
    }

    #[test]
    fn same_size_edit_with_restored_mtime_is_seen() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        git_ok(&repo, &["config", "core.trustctime", "false"]);
        git_ok(&repo, &["config", "core.checkStat", "minimal"]);
        let mut ws = GitWorkspace::create(&repo, &run, "loop/m", false).unwrap();
        let file = ws.path().join("keep.txt");
        let old = fs::File::options().write(true).open(&file).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        old.set_modified(past).unwrap();
        let before = ws.snapshot().unwrap();
        fs::write(&file, "KEEP\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(past)
            .unwrap();
        assert_ne!(ws.snapshot().unwrap(), before);
    }

    #[test]
    fn untracked_loops_directory_is_not_a_dirty_repository() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        fs::create_dir_all(repo.join(".loops/workflows")).unwrap();
        fs::write(repo.join(".loops/workflows/w.toml"), "x").unwrap();
        assert!(GitWorkspace::create(&repo, &run, "loop/l", false).is_ok());
    }

    #[test]
    fn repositories_inside_ignored_directories_are_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        let mut ws = GitWorkspace::create(&repo, &run, "loop/i", false).unwrap();
        let dep = ws.path().join("ignored_dir/dep");
        fs::create_dir_all(&dep).unwrap();
        init_repo_with_commit(&dep);
        assert!(ws.snapshot().is_ok());
    }

    #[test]
    fn diff_text_is_bounded() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        let mut ws = GitWorkspace::create(&repo, &run, "loop/d", false).unwrap();
        fs::write(ws.path().join("big.txt"), "linha é longa\n".repeat(400_000)).unwrap();
        let tree = ws.snapshot().unwrap();
        let diff = ws.diff(&tree).unwrap();
        assert!(diff.len() <= MAX_DIFF_BYTES + 100);
        assert!(diff.ends_with("[tchelvis: diff truncado]\n"));
        assert_eq!(ws.changed_files(&tree).unwrap(), ["big.txt"]);
    }

    #[test]
    fn preflight_check_creates_nothing_and_create_in_separates_workspace_from_run_dir() {
        let tmp = TempDir::new().unwrap();
        let (repo, run_dir) = new_repo(&tmp);
        let ws_dir = tmp.path().join("state/loops/run1/workspace");

        // Dirty repo fails read-only preflight without creating run_dir or ws_dir
        fs::write(repo.join("keep.txt"), "dirty\n").unwrap();
        assert!(GitWorkspace::preflight_check_repo(&repo, "loop/sep", false).is_err());
        assert!(!run_dir.exists());
        assert!(!ws_dir.exists());

        // Restore clean state, run preflight -> still creates nothing
        fs::write(repo.join("keep.txt"), "keep\n").unwrap();
        let info = GitWorkspace::preflight_check_repo(&repo, "loop/sep", false).unwrap();
        assert!(!run_dir.exists());
        assert!(!ws_dir.exists());

        // create_in places workspace separately from run_dir and FrozenWorkspaceMeta works without repo .loops
        let ws = GitWorkspace::create_in(info, &run_dir, &ws_dir).unwrap();
        assert_eq!(ws.path(), ws_dir.as_path());
        assert!(ws_dir.join("keep.txt").is_file());
        assert!(run_dir.join("engine.index").is_file());
        assert!(!run_dir.join("workspace").exists());

        let frozen = ws.to_frozen();
        let mut ws2 = GitWorkspace::from_frozen(frozen);
        fs::write(ws2.path().join("added.txt"), "hi\n").unwrap();
        let tree = ws2.snapshot().unwrap();
        ws2.publish(&tree, "loop test").unwrap();

        let head_branch = git_ok(&repo, &["rev-parse", "--verify", "loop/sep"]);
        assert!(!head_branch.trim().is_empty());
    }

    /// A clean/fsmonitor command that leaves a marker, configured in the repository at `git_dir`.
    fn make_hostile(dir: &Path, marker: &Path) {
        let cmd = format!("touch '{}'; cat", marker.display());
        git_ok(dir, &["config", "filter.evil.clean", &cmd]);
        git_ok(dir, &["config", "core.fsmonitor", &cmd]);
        let git_dir = PathBuf::from(git_ok(dir, &["rev-parse", "--absolute-git-dir"]));
        fs::create_dir_all(git_dir.join("info")).unwrap();
        fs::write(git_dir.join("info/attributes"), "*.txt filter=evil\n").unwrap();
    }

    fn repo_with_submodule(tmp: &TempDir) -> (PathBuf, PathBuf, PathBuf) {
        let (repo, run) = new_repo(tmp);
        let inner = tmp.path().join("inner");
        fs::create_dir_all(&inner).unwrap();
        init_repo_with_commit(&inner);
        let url = inner.to_str().unwrap();
        let add = ["-c", "protocol.file.allow=always", "submodule", "add", url];
        git_ok(&repo, &[&add[..], &["sm"]].concat());
        git_ok(&repo, &["commit", "-m", "submodule"]);
        (repo, run, inner)
    }

    #[test]
    fn a_repository_planted_in_a_submodule_directory_runs_nothing() {
        for recurse in ["false", "true"] {
            let tmp = TempDir::new().unwrap();
            let (repo, run, inner) = repo_with_submodule(&tmp);
            git_ok(&repo, &["config", "submodule.recurse", recurse]);
            let modules = repo.join(".git/modules/sm");
            let config_before = fs::read_to_string(modules.join("config")).unwrap();
            let marker = tmp.path().join("pwned.marker");
            let mut ws = GitWorkspace::create(&repo, &run, "loop/sm", false).unwrap();
            let base = ws.snapshot().unwrap();

            let sm = ws.path().join("sm");
            git_ok(ws.path(), &["clone", "-q", inner.to_str().unwrap(), "sm"]);
            make_hostile(&sm, &marker);
            fs::write(sm.join("keep.txt"), "KEEP\n").unwrap();

            let err = ws.snapshot().unwrap_err().to_string();
            assert!(err.contains("submódulo"), "{err}");
            let _ = ws.restore(&base);
            assert!(!marker.exists(), "recurse={recurse}");
            assert_eq!(
                fs::read_to_string(modules.join("config")).unwrap(),
                config_before
            );
        }
    }

    #[test]
    fn gitlinks_with_a_path_that_is_not_utf8_are_refused() {
        let oid = "a".repeat(40);
        let line = |mode: &str, rest: &str| {
            let mut raw = format!("{mode} {rest}\t").into_bytes();
            raw.extend_from_slice(b"\xff\0");
            raw
        };
        assert!(parse_gitlinks_from_ls_tree(&line("160000", &format!("commit {oid}"))).is_err());
        assert!(parse_gitlinks_from_ls_files_stage(&line("160000", &format!("{oid} 0"))).is_err());
        // An ordinary file may have any name.
        assert!(
            parse_gitlinks_from_ls_tree(&line("100644", &format!("blob {oid}")))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_removed_submodule_is_diffed_without_entering_it() {
        let tmp = TempDir::new().unwrap();
        let (repo, run, _) = repo_with_submodule(&tmp);
        git_ok(&repo, &["config", "diff.submodule", "diff"]);
        let mut ws = GitWorkspace::create(&repo, &run, "loop/sm", false).unwrap();
        fs::remove_dir(ws.path().join("sm")).unwrap();
        let tree = ws.snapshot().unwrap();
        assert!(!ws.diff(&tree).unwrap().contains("hello"));
    }

    #[test]
    fn the_status_check_runs_no_filter_of_the_repository_or_of_a_submodule() {
        let tmp = TempDir::new().unwrap();
        let (repo, _run, _) = repo_with_submodule(&tmp);
        let marker = tmp.path().join("pwned.marker");

        // The submodule's own configuration.
        make_hostile(&repo.join("sm"), &marker);
        fs::write(repo.join("sm/keep.txt"), "KEEP\n").unwrap();
        let _ = GitWorkspace::preflight_check_repo(&repo, "loop/s", false);
        assert!(!marker.exists(), "submodule");

        // The repository's: refused, and without running it first.
        make_hostile(&repo, &marker);
        fs::write(repo.join("keep.txt"), "KEEP\n").unwrap();
        let err = GitWorkspace::preflight_check_repo(&repo, "loop/s", false).unwrap_err();
        assert!(err.to_string().contains("evil"), "{err:#}");
        assert!(!marker.exists(), "repository");
    }

    #[test]
    fn only_the_exact_git_lfs_commands_are_trusted() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        fs::write(repo.join(".gitattributes"), "keep.txt filter=lfs\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "attr"]);
        git_ok(
            &repo,
            &["config", "filter.lfs.process", "git-lfs filter-process"],
        );
        git_ok(
            &repo,
            &["config", "filter.lfs.smudge", "git-lfs smudge -- %f"],
        );
        for bad in [
            "git-lfs clean -- %f; touch pwned",
            "git-lfs clean -- %f && x",
            "git-lfs $(x)",
            "git-lfs clean -- %f\nx",
            "git-lfs  clean",
        ] {
            git_ok(&repo, &["config", "filter.lfs.clean", bad]);
            assert!(
                GitWorkspace::preflight_check_repo(&repo, "loop/l", true).is_err(),
                "{bad}"
            );
        }
        for good in ["git-lfs clean -- %f", "git-lfs clean %f"] {
            git_ok(&repo, &["config", "filter.lfs.clean", good]);
            GitWorkspace::preflight_check_repo(&repo, "loop/l", true).unwrap();
        }
        let _ = run;
    }

    fn configure_lfs(repo: &Path) {
        git_ok(repo, &["config", "filter.lfs.clean", "git-lfs clean -- %f"]);
        git_ok(
            repo,
            &["config", "filter.lfs.smudge", "git-lfs smudge -- %f"],
        );
        git_ok(
            repo,
            &["config", "filter.lfs.process", "git-lfs filter-process"],
        );
        git_ok(repo, &["config", "filter.lfs.required", "true"]);
    }

    /// Commits a root `.gitattributes` (`*.bin filter=lfs`) and `files`, creating parent directories.
    fn commit_lfs_files(repo: &Path, files: &[(&str, &str)]) {
        fs::write(repo.join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        for (path, content) in files {
            let file = repo.join(path);
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(file, content).unwrap();
        }
        git_ok(repo, &["add", "."]);
        git_ok(repo, &["commit", "-m", "lfs files"]);
    }

    /// `new_repo` + `commit_lfs_files` + `configure_lfs` (so the filter is in `.git/config` only).
    fn lfs_repo(tmp: &TempDir, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let (repo, run) = new_repo(tmp);
        commit_lfs_files(&repo, files);
        configure_lfs(&repo);
        (repo, run)
    }

    #[test]
    fn lfs_commands_keep_the_last_value_and_never_skip() {
        let settings = vec![
            ("smudge".to_string(), "git-lfs smudge %f".to_string()),
            (
                "smudge".to_string(),
                "git-lfs smudge --skip -- %f".to_string(),
            ),
            (
                "process".to_string(),
                "git-lfs filter-process --skip".to_string(),
            ),
            ("clean".to_string(), "git-lfs clean -- %f".to_string()),
        ];
        let expected: BTreeMap<String, String> = [
            ("clean", "git-lfs clean -- %f"),
            ("process", "git-lfs filter-process"),
            ("smudge", "git-lfs smudge -- %f"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(lfs_checkout_commands(&settings), Some(expected));
    }

    #[test]
    fn lfs_commands_need_smudge_or_process() {
        assert_eq!(
            lfs_checkout_commands(&[("clean".to_string(), "git-lfs clean -- %f".to_string())]),
            None
        );
        let commands = lfs_checkout_commands(&[
            ("clean".to_string(), "git-lfs clean -- %f".to_string()),
            ("smudge".to_string(), "git-lfs smudge %f".to_string()),
        ])
        .expect("smudge is enough");
        assert!(!commands.contains_key("process"));
        assert_eq!(commands["smudge"], "git-lfs smudge %f");
    }

    #[test]
    fn preflight_records_the_lfs_filter_and_its_local_store() {
        let tmp = TempDir::new().unwrap();
        let (repo, _run) = lfs_repo(&tmp, &[("a.bin", "raw\n")]);
        let info = GitWorkspace::preflight_check_repo(&repo, "loop/lfs", true).unwrap();
        assert!(info.lfs_enabled());
        let lfs = info.lfs.as_ref().unwrap();
        assert_eq!(lfs.commands["smudge"], "git-lfs smudge -- %f");
        assert_eq!(lfs.storage, repo.join(".git/lfs"));
        assert!(!info.disabled_filters.iter().any(|f| f == "lfs"));
    }

    #[test]
    fn lfs_configured_but_not_used_is_disabled() {
        let tmp = TempDir::new().unwrap();
        let (repo, _run) = new_repo(&tmp);
        configure_lfs(&repo);
        let info = GitWorkspace::preflight_check_repo(&repo, "loop/lfs", true).unwrap();
        assert!(info.lfs.is_none());
        assert!(info.disabled_filters.contains(&"lfs".to_string()));
    }

    #[test]
    fn lfs_store_of_a_linked_worktree_is_the_common_one() {
        let tmp = TempDir::new().unwrap();
        let (repo, _run) = new_repo(&tmp);
        commit_lfs_files(&repo, &[("a.bin", "raw\n")]);
        let wt = tmp.path().join("wt");
        git_ok(
            &repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
        );
        // Only now: with the filter configured, `worktree add` would run git-lfs.
        configure_lfs(&repo);
        let info = GitWorkspace::preflight_check_repo(&wt, "loop/wt", true).unwrap();
        fs::create_dir_all(repo.join(".git/lfs")).unwrap();
        let storage = fs::canonicalize(info.lfs.unwrap().storage).unwrap();
        assert_eq!(storage, fs::canonicalize(repo.join(".git/lfs")).unwrap());
    }

    #[test]
    fn configured_lfs_storage_is_respected() {
        let tmp = TempDir::new().unwrap();
        let (repo, _run) = lfs_repo(&tmp, &[("a.bin", "raw\n")]);
        let storage = |repo: &Path| {
            GitWorkspace::preflight_check_repo(repo, "loop/s", true)
                .unwrap()
                .lfs
                .unwrap()
                .storage
        };
        git_ok(&repo, &["config", "lfs.storage", "store"]);
        assert_eq!(storage(&repo), repo.join(".git/store"));
        let abs = tmp.path().join("abs");
        git_ok(&repo, &["config", "lfs.storage", abs.to_str().unwrap()]);
        assert_eq!(storage(&repo), abs);
    }

    #[test]
    fn untrusted_lfs_smudge_is_refused() {
        let tmp = TempDir::new().unwrap();
        let (repo, _run) = lfs_repo(&tmp, &[("a.bin", "raw\n")]);
        git_ok(&repo, &["config", "filter.lfs.smudge", "./evil"]);
        let Err(err) = GitWorkspace::preflight_check_repo(&repo, "loop/lfs", true) else {
            panic!("expected the untrusted smudge to be refused");
        };
        let err = err.to_string();
        assert!(err.contains("'lfs'"), "{err}");
    }

    #[test]
    fn old_frozen_metadata_loads_without_lfs_enabled() {
        let tmp = TempDir::new().unwrap();
        let (repo, run) = new_repo(&tmp);
        let ws = GitWorkspace::create(&repo, &run, "loop/frozen", false).unwrap();
        let frozen = ws.to_frozen();
        let mut value = serde_json::to_value(&frozen).unwrap();
        value.as_object_mut().unwrap().remove("lfs_enabled");
        let loaded: FrozenWorkspaceMeta = serde_json::from_value(value).unwrap();
        assert!(!loaded.lfs_enabled);
        assert_eq!(loaded, frozen);
    }

    fn pointer(oid: &str, size: u64) -> String {
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
    }

    fn put_lfs_object(storage: &Path, oid: &str, size: usize) {
        let dir = storage.join("objects").join(&oid[..2]).join(&oid[2..4]);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(oid), vec![b'x'; size]).unwrap();
    }

    #[test]
    fn lfs_blobs_are_classified() {
        let oid = "ab".repeat(32);
        assert_eq!(
            classify_lfs_blob(pointer(&oid, 12).as_bytes()),
            LfsBlob::Pointer {
                oid: oid.clone(),
                size: 12
            }
        );
        let canonical = pointer(&oid, 12);
        let non_canonical = [
            canonical.replace(
                "https://git-lfs.github.com/spec/v1",
                "https://hawser.github.com/spec/v1",
            ),
            canonical.replace('\n', "\r\n"),
            format!("version https://git-lfs.github.com/spec/v1\next-0-x sha256:{oid}\noid sha256:{oid}\nsize 12\n"),
            pointer(&oid, 0),
            pointer(&oid, 12).replace("size 12", "size 012"),
            canonical.trim_end().to_string(),
            pointer(&oid.to_uppercase(), 12),
        ];
        for blob in non_canonical {
            assert_eq!(
                classify_lfs_blob(blob.as_bytes()),
                LfsBlob::NonCanonical,
                "{blob:?}"
            );
        }
        for blob in [b"raw\n".as_slice(), b"", b"\xff\xfe"] {
            assert_eq!(classify_lfs_blob(blob), LfsBlob::Content, "{blob:?}");
        }
    }

    #[test]
    fn lfs_summary_lists_three_paths() {
        let paths = |names: &[&str]| -> Vec<Vec<u8>> {
            names.iter().map(|n| n.as_bytes().to_vec()).collect()
        };
        assert_eq!(lfs_paths_summary(&paths(&["a", "b"])), "a, b");
        assert_eq!(
            lfs_paths_summary(&paths(&["a", "b", "c", "d", "e"])),
            "a, b, c e mais 2"
        );
        assert_eq!(lfs_paths_summary(&paths(&["a\x1b"])), "a\\u{1b}");
    }

    #[test]
    fn missing_lfs_object_is_refused_before_anything_is_created() {
        let tmp = TempDir::new().unwrap();
        let oids: Vec<String> = ["11", "22", "33", "44"]
            .iter()
            .map(|prefix| prefix.repeat(32))
            .collect();
        let files: Vec<(String, String)> = oids
            .iter()
            .enumerate()
            .map(|(i, oid)| (format!("f{i}.bin"), pointer(oid, 5)))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_str()))
            .collect();
        let (repo, run) = lfs_repo(&tmp, &refs);
        put_lfs_object(&repo.join(".git/lfs"), &oids[0], 5);

        let Err(err) = GitWorkspace::create(&repo, &run, "loop/lfs", true) else {
            panic!("expected missing LFS objects to be refused");
        };
        assert_eq!(
            err.to_string(),
            "faltam 3 objeto(s) do Git LFS no repositório: f1.bin, f2.bin, f3.bin; rode 'git lfs fetch' e tente de novo"
        );
        assert!(!run.exists());
    }

    #[test]
    fn present_lfs_objects_pass_preflight() {
        let tmp = TempDir::new().unwrap();
        let oid_a = "aa".repeat(32);
        let oid_b = "bb".repeat(32);
        let files = [
            ("a.bin", pointer(&oid_a, 3)),
            ("dup.bin", pointer(&oid_a, 3)),
            ("sub/.gitattributes", "*.dat filter=lfs\n".to_string()),
            ("sub/x.dat", pointer(&oid_b, 7)),
        ];
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (*path, content.as_str()))
            .collect();
        let (repo, _run) = lfs_repo(&tmp, &refs);
        let storage = repo.join(".git/lfs");
        put_lfs_object(&storage, &oid_a, 3);
        put_lfs_object(&storage, &oid_b, 7);
        GitWorkspace::preflight_check_repo(&repo, "loop/lfs", true).unwrap();
    }

    #[test]
    fn truncated_lfs_object_is_refused_and_kept() {
        let tmp = TempDir::new().unwrap();
        let oid = "cc".repeat(32);
        let content = pointer(&oid, 5);
        let (repo, run) = lfs_repo(&tmp, &[("a.bin", content.as_str())]);
        let storage = repo.join(".git/lfs");
        put_lfs_object(&storage, &oid, 4);
        let Err(err) = GitWorkspace::create(&repo, &run, "loop/lfs", true) else {
            panic!("expected the truncated object to be refused");
        };
        assert!(err.to_string().contains("faltam 1"), "{err}");
        let object = storage
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(&oid);
        assert!(object.exists(), "the local object must be kept");
    }

    #[test]
    fn pointer_text_outside_the_lfs_attribute_is_not_required() {
        let tmp = TempDir::new().unwrap();
        let oid = "dd".repeat(32);
        let fixture = pointer(&oid, 9);
        let (repo, _run) = lfs_repo(&tmp, &[("fixture.txt", fixture.as_str())]);
        GitWorkspace::preflight_check_repo(&repo, "loop/lfs", true).unwrap();
    }

    #[test]
    fn lfs_attributes_come_from_the_base_commit() {
        let tmp = TempDir::new().unwrap();
        let missing = "ee".repeat(32);
        let present = "ff".repeat(32);
        let files = [
            ("missing.bin", pointer(&missing, 4)),
            ("x.bin", pointer(&present, 3)),
        ];
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, content)| (*path, content.as_str()))
            .collect();
        let (repo, run) = lfs_repo(&tmp, &refs);
        put_lfs_object(&repo.join(".git/lfs"), &present, 3);
        // The worktree disagrees with the commit; the commit decides.
        fs::write(repo.join(".gitattributes"), "x.bin filter=lfs\n").unwrap();
        let Err(err) = GitWorkspace::create(&repo, &run, "loop/lfs", true) else {
            panic!("expected attributes from the base commit");
        };
        assert!(err.to_string().contains("missing.bin"), "{err}");
    }

    #[test]
    fn non_canonical_lfs_pointer_is_refused() {
        let tmp = TempDir::new().unwrap();
        let oid = "12".repeat(32);
        let hawser = pointer(&oid, 5).replace(
            "https://git-lfs.github.com/spec/v1",
            "https://hawser.github.com/spec/v1",
        );
        let (repo, run) = lfs_repo(&tmp, &[("a.bin", hawser.as_str())]);
        put_lfs_object(&repo.join(".git/lfs"), &oid, 5);
        let Err(err) = GitWorkspace::create(&repo, &run, "loop/lfs", true) else {
            panic!("expected the non-canonical pointer to be refused");
        };
        assert!(
            err.to_string()
                .contains("1 ponteiro(s) do Git LFS fora do formato canônico"),
            "{err}"
        );
    }

    #[test]
    fn lfs_store_objects_are_found_from_a_linked_worktree() {
        let tmp = TempDir::new().unwrap();
        let oid = "34".repeat(32);
        let (repo, _run) = new_repo(&tmp);
        commit_lfs_files(&repo, &[("a.bin", &pointer(&oid, 5))]);
        let wt = tmp.path().join("wt");
        git_ok(
            &repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
        );
        configure_lfs(&repo);
        put_lfs_object(&repo.join(".git/lfs"), &oid, 5);
        GitWorkspace::preflight_check_repo(&wt, "loop/wt", true).unwrap();
    }

    fn git_lfs_available() -> bool {
        Command::new("git-lfs")
            .arg("version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// A repository whose `a.bin` is a real LFS file, with the filter only in `.git/config`.
    fn real_lfs_repo(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let (repo, run) = new_repo(tmp);
        configure_lfs(&repo);
        fs::write(
            repo.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        fs::write(repo.join("a.bin"), b"REAL\0CONTENT").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "real lfs"]);
        let blob = git_ok(&repo, &["cat-file", "-p", "HEAD:a.bin"]);
        assert!(blob.starts_with("version https://git-lfs"), "{blob}");
        (repo, run)
    }

    fn lfs_checkout(commands: &[(&str, &str)], storage: &Path) -> LfsCheckout {
        LfsCheckout {
            commands: commands
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            storage: storage.to_path_buf(),
        }
    }

    #[test]
    fn lfs_checkout_config_is_exact() {
        let lfs = lfs_checkout(
            &[
                ("clean", "git-lfs clean -- %f"),
                ("process", "git-lfs filter-process"),
                ("smudge", "git-lfs smudge -- %f"),
            ],
            Path::new("/store"),
        );
        let expected: Vec<(String, String)> = [
            ("filter.lfs.clean", "git-lfs clean -- %f"),
            ("filter.lfs.process", "git-lfs filter-process"),
            ("filter.lfs.smudge", "git-lfs smudge -- %f"),
            ("filter.lfs.required", "true"),
            ("lfs.storage", "/store"),
            ("lfs.url", "file:///dev/null"),
            ("lfs.fetchinclude", ""),
            ("lfs.fetchexclude", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(lfs_checkout_config(&lfs), expected);
    }

    #[test]
    fn disabled_filters_and_lfs_settings_do_not_overwrite_each_other() {
        let tmp = TempDir::new().unwrap();
        let lfs = lfs_checkout(
            &[("smudge", "git-lfs smudge -- %f")],
            &tmp.path().join("store"),
        );
        let mut config = disabled_filter_config(&["a=b".to_string()]);
        config.extend(lfs_checkout_config(&lfs));

        let get = |key: &str| -> String {
            let mut cmd = Command::new("git");
            cmd.args(["config", "--get", key])
                .current_dir(tmp.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
            set_git_config(&mut cmd, &config);
            let out = cmd.output().unwrap();
            assert!(
                out.status.success(),
                "git config --get {key}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(get("filter.a=b.required"), "false");
        assert_eq!(get("lfs.storage"), lfs.storage.to_str().unwrap());
        assert_eq!(get("lfs.url"), "file:///dev/null");
    }

    #[test]
    fn lfs_files_are_real_content_and_the_snapshot_keeps_the_tree() {
        if !git_lfs_available() {
            println!("SKIPPED: git-lfs unavailable");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (repo, run) = real_lfs_repo(&tmp);
        let mut ws = GitWorkspace::create(&repo, &run, "loop/lfs", false).unwrap();
        assert_eq!(fs::read(ws.path().join("a.bin")).unwrap(), b"REAL\0CONTENT");
        assert_eq!(ws.snapshot().unwrap(), ws.base_tree());
        assert!(ws.to_frozen().lfs_enabled);

        let base_tree = ws.base_tree().to_string();
        fs::write(ws.path().join("a.bin"), b"NEW").unwrap();
        let tree = ws.snapshot().unwrap();
        assert_ne!(tree, base_tree);
        let blob = git_cmd(&repo, &["cat-file", "-p", &format!("{tree}:a.bin")]).stdout;
        match classify_lfs_blob(&blob) {
            LfsBlob::Pointer { size, .. } => assert_eq!(size, 3),
            other => panic!("esperava um ponteiro, veio {other:?}"),
        }
        ws.restore(&base_tree).unwrap();
        assert_eq!(fs::read(ws.path().join("a.bin")).unwrap(), b"REAL\0CONTENT");
    }

    #[test]
    fn lfs_content_appears_for_every_trusted_command_form() {
        if !git_lfs_available() {
            println!("SKIPPED: git-lfs unavailable");
            return;
        }
        for form in ["process-unset", "skip"] {
            let tmp = TempDir::new().unwrap();
            let (repo, run) = real_lfs_repo(&tmp);
            match form {
                "process-unset" => {
                    git_ok(&repo, &["config", "--unset", "filter.lfs.process"]);
                }
                _ => {
                    git_ok(
                        &repo,
                        &["config", "filter.lfs.smudge", "git-lfs smudge --skip -- %f"],
                    );
                    git_ok(
                        &repo,
                        &[
                            "config",
                            "filter.lfs.process",
                            "git-lfs filter-process --skip",
                        ],
                    );
                }
            }
            let mut ws = GitWorkspace::create(&repo, &run, "loop/lfs", false).unwrap();
            assert_eq!(
                fs::read(ws.path().join("a.bin")).unwrap(),
                b"REAL\0CONTENT",
                "{form}"
            );
            assert_eq!(ws.snapshot().unwrap(), ws.base_tree(), "{form}");
        }
    }

    #[test]
    fn repository_lfsconfig_chooses_nothing() {
        if !git_lfs_available() {
            println!("SKIPPED: git-lfs unavailable");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (repo, run) = real_lfs_repo(&tmp);
        fs::write(
            repo.join(".lfsconfig"),
            format!("[lfs]\n\turl = http://127.0.0.1:{port}/x\n\tfetchexclude = *.bin\n"),
        )
        .unwrap();
        git_ok(&repo, &["add", ".lfsconfig"]);
        git_ok(&repo, &["commit", "-m", "lfsconfig"]);

        let ws = GitWorkspace::create(&repo, &run, "loop/lfs", false).unwrap();
        assert_eq!(fs::read(ws.path().join("a.bin")).unwrap(), b"REAL\0CONTENT");

        let info = GitWorkspace::preflight_check_repo(&repo, "loop/lfs2", false).unwrap();
        let pointer = git_cmd(&repo, &["cat-file", "-p", "HEAD:a.bin"]).stdout;
        let LfsBlob::Pointer { oid, .. } = classify_lfs_blob(&pointer) else {
            panic!("esperava um ponteiro");
        };
        let object = repo
            .join(".git/lfs/objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(&oid);
        fs::remove_file(&object).unwrap();
        let run2 = tmp.path().join("run2");
        // Preflight passed before the object was gone; the checkout must still fail without asking
        // the repository's `.lfsconfig` URL, which `lfs.url` overrides.
        assert!(
            GitWorkspace::create_in(info, &run2, &run2.join("workspace")).is_err(),
            "esperava a falha do checkout sem objeto"
        );
        match listener.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("o checkout não deveria contatar a URL: {other:?}"),
        }
    }

    #[test]
    fn lfs_checkout_works_from_a_linked_worktree() {
        if !git_lfs_available() {
            println!("SKIPPED: git-lfs unavailable");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (repo, _run) = real_lfs_repo(&tmp);
        let wt = tmp.path().join("wt");
        git_ok(
            &repo,
            &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
        );
        let run = tmp.path().join("run-wt");
        let mut ws = GitWorkspace::create(&wt, &run, "loop/wt", false).unwrap();
        assert_eq!(fs::read(ws.path().join("a.bin")).unwrap(), b"REAL\0CONTENT");
        assert_eq!(ws.snapshot().unwrap(), ws.base_tree());
    }

    #[test]
    fn lfs_attribute_without_a_filter_is_unchanged() {
        if !git_lfs_available() {
            println!("SKIPPED: git-lfs unavailable");
            return;
        }
        let probe = TempDir::new().unwrap();
        let found = Command::new("git")
            .args(["config", "--get-regexp", "^filter\\.lfs\\."])
            .current_dir(probe.path())
            .output()
            .unwrap();
        if found.status.success() && !found.stdout.is_empty() {
            println!("SKIPPED: lfs filter configured globally");
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (repo, run) = real_lfs_repo(&tmp);
        let pointer_text = git_cmd(&repo, &["cat-file", "-p", "HEAD:a.bin"]).stdout;
        let LfsBlob::Pointer { oid, .. } = classify_lfs_blob(&pointer_text) else {
            panic!("esperava um ponteiro");
        };
        git_ok(&repo, &["config", "--remove-section", "filter.lfs"]);
        fs::remove_file(
            repo.join(".git/lfs/objects")
                .join(&oid[..2])
                .join(&oid[2..4])
                .join(&oid),
        )
        .unwrap();

        let mut ws = GitWorkspace::create(&repo, &run, "loop/lfs", true).unwrap();
        assert!(!ws.to_frozen().lfs_enabled);
        assert_eq!(fs::read(ws.path().join("a.bin")).unwrap(), pointer_text);
        assert_eq!(ws.snapshot().unwrap(), ws.base_tree());
    }
}
