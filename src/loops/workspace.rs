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

#[derive(Debug, Clone)]
pub struct RepoPreflightInfo {
    pub repo_root: PathBuf,
    pub main_git_dir: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub base_tree: String,
    pub disabled_filters: Vec<String>,
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

/// Names of every clean/smudge filter configured for the repository (any scope).
fn configured_filters(git_dir: &Path) -> Result<BTreeMap<String, Vec<String>>> {
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
    let mut filters: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8(entry.to_vec()).context("filtro git com nome inválido")?;
        let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
        let name = key
            .strip_prefix("filter.")
            .and_then(|k| k.rsplit_once('.'))
            .map(|(name, _)| name)
            .with_context(|| format!("filtro git inesperado: {key:?}"))?;
        filters
            .entry(name.to_string())
            .or_default()
            .push(value.to_string());
    }
    Ok(filters)
}

/// Filters that files of `repo_root` use through attributes.
fn filters_in_use(repo_root: &Path) -> Result<BTreeSet<String>> {
    let mut ls = repo_git_cmd(repo_root);
    ls.args(["ls-files", "-z"]);
    let files = run_git_checked(ls, "falha ao listar arquivos do repositório")?.stdout;
    let mut check = repo_git_cmd(repo_root);
    check
        .args(["check-attr", "-z", "--stdin", "filter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = check.spawn().context("falha ao ler atributos do git")?;
    let mut stdin = child.stdin.take().context("stdin de check-attr ausente")?;
    let writer = std::thread::spawn(move || stdin.write_all(&files));
    let out = child
        .wait_with_output()
        .context("falha ao ler atributos do git")?;
    let _ = writer.join();
    if !out.status.success() {
        bail!("falha ao ler atributos do git");
    }
    let fields: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
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

/// A filter command runs outside the sandbox with the workspace as its directory, over files the
/// agents wrote. Only Git LFS is trusted, and only where the repository already uses it. Every
/// other filter is switched off; a repository that depends on one is refused, because storing its
/// files unfiltered would change (or, for git-crypt, disclose) their content.
fn filters_to_disable(git_dir: &Path, repo_root: &Path) -> Result<Vec<String>> {
    let in_use = filters_in_use(repo_root)?;
    let mut disable = Vec::new();
    for (name, commands) in configured_filters(git_dir)? {
        let is_lfs = name == "lfs"
            && commands
                .iter()
                .all(|c| GIT_LFS_COMMANDS.contains(&c.as_str()));
        match (in_use.contains(&name), is_lfs) {
            (true, true) => {}
            (true, false) => bail!(
                "o repositório usa o filtro git '{name}', que executaria fora do sandbox; não suportado pelo loop"
            ),
            (false, _) => disable.push(name),
        }
    }
    Ok(disable)
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

/// Through `GIT_CONFIG_*` and not `-c`: a filter name may contain `=`.
fn set_disabled_filters(cmd: &mut Command, filters: &[String]) {
    let mut count = 0;
    for name in filters {
        for (key, value) in [
            ("clean", ""),
            ("smudge", ""),
            ("process", ""),
            ("required", "false"),
        ] {
            cmd.env(
                format!("GIT_CONFIG_KEY_{count}"),
                format!("filter.{name}.{key}"),
            )
            .env(format!("GIT_CONFIG_VALUE_{count}"), value);
            count += 1;
        }
    }
    cmd.env("GIT_CONFIG_COUNT", count.to_string());
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
        let disabled_filters = filters_to_disable(&main_git_dir, &repo_root)?;

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

        Ok(RepoPreflightInfo {
            repo_root,
            main_git_dir,
            branch: branch.to_string(),
            base_commit,
            base_tree,
            disabled_filters,
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
        let mut co_cmd = own_git();
        co_cmd.args(["checkout", "-q", "--detach", &info.base_commit]);
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
}
