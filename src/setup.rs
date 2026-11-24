use crate::assets::{
    asset_sha256, bytes_sha256, CLAUDE_HOOK_MARKER, CLAUDE_HOOK_SH, FENCE_END, FENCE_START,
    HERDR_LAYOUT_JSON, HERDR_MOD_ICONS, HERDR_MOD_JSON, HERDR_MOD_MAIN_JS, OPENCODE_TCHELVIS_JS,
    PI_TCHELVIS_JS, SHELL_FISH, SHELL_NU, SHELL_SH, SYSTEMD_UNIT_TEMPLATE, TO_TICKETS_SKILL,
    TO_TICKETS_SKILL_NAME,
};
use crate::config::CockpitPaths;
use crate::orch::preamble::{
    orchestration_opencode_command_markdown, orchestration_skill_file_markdown, ORCH_SKILL_NAME,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const VALID_TARGETS: [&str; 7] = [
    "shell",
    "claude",
    "opencode",
    "pi",
    "herdr-mod",
    "skills",
    "systemd",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestActionKind {
    CreatedFile,
    FencedBlock,
    JsonMerge {
        #[serde(default)]
        previous_value: Option<Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub existed_before: bool,
    #[serde(default)]
    pub pre_sha256: Option<String>,
    pub post_sha256: String,
    pub action: ManifestActionKind,
    #[serde(default)]
    pub keep_if_modified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupManifest {
    pub version: u32,
    pub updated_at: i64,
    pub entries: Vec<ManifestEntry>,
}

impl Default for SetupManifest {
    fn default() -> Self {
        Self {
            version: 1,
            updated_at: chrono::Utc::now().timestamp(),
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    pub dry_run: bool,
    pub only: Option<String>,
    pub skip: Option<String>,
    pub verify_only: bool,
    pub undo: bool,
    pub purge: bool,
    pub set_default_layout: bool,
    pub systemd: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupReport {
    pub dry_run: bool,
    pub action: String,
    pub planned_or_applied: Vec<String>,
    pub verified_ok: bool,
}

pub fn resolve_active_targets(opts: &SetupOptions) -> Result<BTreeSet<String>> {
    let mut set: BTreeSet<String> = if let Some(ref only_csv) = opts.only {
        let mut s = BTreeSet::new();
        for item in only_csv
            .split(',')
            .map(|x| x.trim())
            .filter(|x| !x.is_empty())
        {
            if !VALID_TARGETS.contains(&item) {
                bail!(
                    "Invalid setup target '{}'. Valid targets: {}",
                    item,
                    VALID_TARGETS.join(", ")
                );
            }
            s.insert(item.to_string());
        }
        s
    } else {
        ["shell", "claude", "opencode", "pi", "herdr-mod", "skills"]
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    };

    if opts.systemd {
        set.insert("systemd".to_string());
    }

    if let Some(ref skip_csv) = opts.skip {
        for item in skip_csv
            .split(',')
            .map(|x| x.trim())
            .filter(|x| !x.is_empty())
        {
            if !VALID_TARGETS.contains(&item) {
                bail!(
                    "Invalid skip target '{}'. Valid targets: {}",
                    item,
                    VALID_TARGETS.join(", ")
                );
            }
            set.remove(item);
        }
    }

    Ok(set)
}

pub fn run_setup(paths: &CockpitPaths, opts: &SetupOptions) -> Result<SetupReport> {
    run_setup_with(paths, opts, &mut |_| false)
}

pub fn run_setup_with(
    paths: &CockpitPaths,
    opts: &SetupOptions,
    confirm: &mut dyn FnMut(&Path) -> bool,
) -> Result<SetupReport> {
    if opts.undo {
        return run_uninstall(paths, opts.dry_run, opts.purge, None);
    }
    if opts.verify_only {
        return verify_installation(paths);
    }

    let targets = resolve_active_targets(opts)?;
    let mut actions = Vec::new();
    let mut manifest = load_manifest(paths).ok().flatten().unwrap_or_default();

    let hooks_dir = paths.hooks_dir();
    let hook_files: Vec<(PathBuf, &str, u32, &str)> = vec![
        (hooks_dir.join("shell.sh"), SHELL_SH, 0o644, "shell"),
        (hooks_dir.join("shell.fish"), SHELL_FISH, 0o644, "shell"),
        (hooks_dir.join("shell.nu"), SHELL_NU, 0o644, "shell"),
        (
            hooks_dir.join("claude-hook.sh"),
            CLAUDE_HOOK_SH,
            0o755,
            "claude",
        ),
        (
            hooks_dir.join("opencode-tchelvis.js"),
            OPENCODE_TCHELVIS_JS,
            0o644,
            "opencode",
        ),
        (
            hooks_dir.join("pi-tchelvis.js"),
            PI_TCHELVIS_JS,
            0o644,
            "pi",
        ),
    ];

    for (path, content, mode, target_name) in hook_files {
        if !targets.contains(target_name) {
            continue;
        }
        apply_created_file(
            &path,
            content,
            mode,
            opts.dry_run,
            &mut manifest,
            &mut actions,
        )?;
    }

    // 1. Shell RC fenced block edits
    if targets.contains("shell") {
        let shell_sh_path = hooks_dir.join("shell.sh");
        let sh_snippet = format!(
            "{FENCE_START}\n[ -f \"{}\" ] && . \"{}\"\n{FENCE_END}",
            shell_sh_path.display(),
            shell_sh_path.display()
        );
        for rc_name in [".bashrc", ".zshrc"] {
            let rc_path = paths.home_dir.join(rc_name);
            if rc_path.exists() {
                apply_fenced_block(
                    &rc_path,
                    &sh_snippet,
                    opts.dry_run,
                    &mut manifest,
                    &mut actions,
                )?;
            }
        }

        let fish_rc = paths.home_dir.join(".config/fish/config.fish");
        if fish_rc.exists() {
            let fish_hook = hooks_dir.join("shell.fish");
            let fish_snippet = format!(
                "{FENCE_START}\nif test -f \"{}\"\n    source \"{}\"\nend\n{FENCE_END}",
                fish_hook.display(),
                fish_hook.display()
            );
            apply_fenced_block(
                &fish_rc,
                &fish_snippet,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }

        let nu_rc = paths.home_dir.join(".config/nushell/config.nu");
        if nu_rc.exists() {
            let nu_hook = hooks_dir.join("shell.nu");
            let nu_snippet = format!(
                "{FENCE_START}\nsource \"{}\"\n{FENCE_END}",
                nu_hook.display()
            );
            apply_fenced_block(
                &nu_rc,
                &nu_snippet,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 2. Claude Code (~/.claude/settings.json)
    if targets.contains("claude") {
        let claude_dir = paths.home_dir.join(".claude");
        if claude_dir.exists() || opts.only.as_deref() == Some("claude") {
            let settings_path = claude_dir.join("settings.json");
            let hook_cmd = format!(
                "{} {}",
                hooks_dir.join("claude-hook.sh").display(),
                CLAUDE_HOOK_MARKER
            );
            apply_claude_settings_merge(
                &settings_path,
                &hook_cmd,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 3. OpenCode (~/.config/opencode/plugins/tchelvis.js)
    if targets.contains("opencode") {
        let opencode_dir = paths.home_dir.join(".config/opencode");
        if opencode_dir.exists() || opts.only.as_deref() == Some("opencode") {
            let plugin_path = opencode_dir.join("plugins/tchelvis.js");
            apply_created_file(
                &plugin_path,
                OPENCODE_TCHELVIS_JS,
                0o644,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 4. Pi (~/.pi/agent/extensions/tchelvis.js)
    if targets.contains("pi") {
        let pi_dir = paths.home_dir.join(".pi/agent");
        if pi_dir.exists() || opts.only.as_deref() == Some("pi") {
            let ext_path = pi_dir.join("extensions/tchelvis.js");
            apply_created_file(
                &ext_path,
                PI_TCHELVIS_JS,
                0o644,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 4b. /orchestration-tchelvis agent skill files (deduplicated across targets)
    {
        let mut skill_files: BTreeMap<PathBuf, String> = BTreeMap::new();
        let skill_md = orchestration_skill_file_markdown();
        let skill_rel = PathBuf::from(format!("skills/{ORCH_SKILL_NAME}/SKILL.md"));

        if targets.contains("claude") || targets.contains("skills") {
            let claude_dir = paths.home_dir.join(".claude");
            if claude_dir.exists() || opts.only.as_deref() == Some("claude") {
                skill_files.insert(claude_dir.join(&skill_rel), skill_md.clone());
            }
        }

        if targets.contains("opencode") || targets.contains("skills") {
            let opencode_dir = paths.home_dir.join(".config/opencode");
            if opencode_dir.exists() || opts.only.as_deref() == Some("opencode") {
                skill_files.insert(opencode_dir.join(&skill_rel), skill_md.clone());
                skill_files.insert(
                    opencode_dir.join(format!("commands/{ORCH_SKILL_NAME}.md")),
                    orchestration_opencode_command_markdown(),
                );
            }
        }

        if targets.contains("pi") || targets.contains("skills") {
            let pi_dir = paths.home_dir.join(".pi/agent");
            if pi_dir.exists() || opts.only.as_deref() == Some("pi") {
                skill_files.insert(pi_dir.join(&skill_rel), skill_md.clone());
            }
        }

        if targets.contains("skills") {
            let agents_dir = paths.home_dir.join(".agents");
            if agents_dir.exists() || opts.only.as_deref() == Some("skills") {
                skill_files.insert(agents_dir.join(&skill_rel), skill_md.clone());
            }
            for agent_dir_name in [".codex", ".gemini"] {
                let agent_dir = paths.home_dir.join(agent_dir_name);
                if agent_dir.exists() {
                    skill_files.insert(agent_dir.join(&skill_rel), skill_md.clone());
                }
            }
        }

        for (path, content) in skill_files {
            apply_created_file(
                &path,
                &content,
                0o644,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 4c. /to-tickets general skill (~/.agents/skills/to-tickets/SKILL.md + Claude link)
    if targets.contains("skills") {
        apply_to_tickets_skill(paths, opts.dry_run, &mut manifest, &mut actions, confirm)?;
    }

    // 5. Herdr Desktop Mod & Layout
    if targets.contains("herdr-mod") {
        let herdr_dir = paths.home_dir.join(".config/herdr-app");
        let had_herdr_dir = herdr_dir.exists();
        let mod_path = herdr_dir.join("mods/tchelvis/main.js");
        let mod_json_path = herdr_dir.join("mods/tchelvis/mod.json");
        let layout_path = herdr_dir.join("layouts/tchelvis.json");
        apply_created_file(
            &mod_path,
            HERDR_MOD_MAIN_JS,
            0o644,
            opts.dry_run,
            &mut manifest,
            &mut actions,
        )?;
        apply_created_file(
            &mod_json_path,
            HERDR_MOD_JSON,
            0o644,
            opts.dry_run,
            &mut manifest,
            &mut actions,
        )?;
        apply_created_file(
            &layout_path,
            HERDR_LAYOUT_JSON,
            0o644,
            opts.dry_run,
            &mut manifest,
            &mut actions,
        )?;
        let icons_dir = herdr_dir.join("mods/tchelvis/icons");
        for (file_name, content) in HERDR_MOD_ICONS {
            let icon_path = icons_dir.join(file_name);
            apply_created_file(
                &icon_path,
                content,
                0o644,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }

        if opts.set_default_layout && (had_herdr_dir || opts.only.as_deref() == Some("herdr-mod")) {
            let herdr_settings = herdr_dir.join("settings.json");
            apply_herdr_default_layout(&herdr_settings, opts.dry_run, &mut manifest, &mut actions)?;
            let herdr_shared = herdr_dir.join("shared.json");
            apply_herdr_shared_layout(&herdr_shared, opts.dry_run, &mut manifest, &mut actions)?;
        }

        cleanup_legacy_supervisor_mods(&paths.home_dir, opts.dry_run, &mut actions);
    }

    // 6. Optional systemd user service
    if targets.contains("systemd") {
        let unit_path = paths.home_dir.join(".config/systemd/user/tchelvis.service");
        apply_created_file(
            &unit_path,
            SYSTEMD_UNIT_TEMPLATE,
            0o644,
            opts.dry_run,
            &mut manifest,
            &mut actions,
        )?;
    }

    if !opts.dry_run {
        manifest.updated_at = chrono::Utc::now().timestamp();
        save_manifest(paths, &manifest)?;
    }

    Ok(SetupReport {
        dry_run: opts.dry_run,
        action: "setup".into(),
        planned_or_applied: actions,
        verified_ok: true,
    })
}

pub fn run_uninstall(
    paths: &CockpitPaths,
    dry_run: bool,
    purge: bool,
    custom_binary_path: Option<&Path>,
) -> Result<SetupReport> {
    let mut actions = Vec::new();
    if let Some(manifest) = load_manifest(paths)? {
        for entry in manifest.entries.iter().rev() {
            let p = PathBuf::from(&entry.path);
            match &entry.action {
                ManifestActionKind::CreatedFile => {
                    if entry.keep_if_modified {
                        if let Ok(meta) = fs::symlink_metadata(&p) {
                            let ft = meta.file_type();
                            let expected_target = paths
                                .home_dir
                                .join(".agents/skills")
                                .join(TO_TICKETS_SKILL_NAME);
                            let linked_parent = p
                                .ancestors()
                                .skip(1)
                                .take_while(|dir| *dir != paths.home_dir)
                                .any(is_symlink_path);
                            let can_remove = if linked_parent {
                                false
                            } else if ft.is_symlink() {
                                fs::read_link(&p)
                                    .map(|target| {
                                        target == expected_target
                                            || asset_sha256(&target.to_string_lossy())
                                                == entry.post_sha256
                                    })
                                    .unwrap_or(false)
                            } else if ft.is_file() {
                                fs::read(&p)
                                    .map(|bytes| bytes_sha256(&bytes) == entry.post_sha256)
                                    .unwrap_or(false)
                            } else {
                                false
                            };
                            if can_remove {
                                actions.push(format!("remove file {}", p.display()));
                                if !dry_run {
                                    let _ = fs::remove_file(&p);
                                }
                            } else {
                                actions.push(format!("keep {} (modified)", p.display()));
                            }
                        }
                    } else if p.exists() {
                        actions.push(format!("remove file {}", p.display()));
                        if !dry_run {
                            let _ = fs::remove_file(&p);
                        }
                    }
                }
                ManifestActionKind::FencedBlock => {
                    if p.exists() {
                        actions.push(format!("remove fenced block from {}", p.display()));
                        if !dry_run {
                            let raw = fs::read_to_string(&p)?;
                            let cleaned = strip_fenced_block(&raw);
                            if !entry.existed_before && cleaned.trim().is_empty() {
                                let _ = fs::remove_file(&p);
                            } else {
                                atomic_write_file(&p, &cleaned, 0o644)?;
                            }
                        }
                    }
                }
                ManifestActionKind::JsonMerge { previous_value } => {
                    if p.exists() {
                        actions.push(format!("revert JSON merge in {}", p.display()));
                        if !dry_run {
                            if !entry.existed_before {
                                let _ = fs::remove_file(&p);
                            } else if let Some(prev) = previous_value {
                                let formatted = serde_json::to_string_pretty(prev)? + "\n";
                                atomic_write_file(&p, &formatted, 0o644)?;
                            } else {
                                revert_claude_hooks_surgically(&p)?;
                            }
                        }
                    }
                }
            }
        }

        if !dry_run {
            let manifest_path = paths.setup_manifest_file();
            if manifest_path.exists() {
                let _ = fs::remove_file(manifest_path);
            }
        }
    } else if !purge {
        actions.push("No setup-manifest.json found; nothing to roll back".into());
    }

    if purge {
        // Fallback cleanup in case setup-manifest.json was already deleted by an earlier `tchelvis uninstall`
        for rc_rel in [
            ".bashrc",
            ".zshrc",
            ".config/fish/config.fish",
            ".config/nushell/config.nu",
        ] {
            let rc_path = paths.home_dir.join(rc_rel);
            if rc_path.exists() {
                if let Ok(raw) = fs::read_to_string(&rc_path) {
                    if raw.contains(FENCE_START) {
                        actions.push(format!("remove fenced block from {}", rc_path.display()));
                        if !dry_run {
                            let cleaned = strip_fenced_block(&raw);
                            let _ = atomic_write_file(&rc_path, &cleaned, 0o644);
                        }
                    }
                }
            }
        }

        let claude_settings = paths.home_dir.join(".claude/settings.json");
        if claude_settings.exists() {
            if let Ok(raw) = fs::read_to_string(&claude_settings) {
                if raw.contains(CLAUDE_HOOK_MARKER) {
                    actions.push(format!(
                        "revert JSON merge in {}",
                        claude_settings.display()
                    ));
                    if !dry_run {
                        let _ = revert_claude_hooks_surgically(&claude_settings);
                    }
                }
            }
        }

        for extra_file in [
            paths
                .home_dir
                .join(".config/herdr-app/layouts/tchelvis.json"),
            paths
                .home_dir
                .join(".config/herdr-app/mods/tchelvis/main.js"),
            paths
                .home_dir
                .join(".config/herdr-app/mods/tchelvis/mod.json"),
            paths.home_dir.join(".pi/agent/extensions/tchelvis.js"),
            paths.home_dir.join(".config/opencode/plugins/tchelvis.js"),
            paths.home_dir.join(".config/systemd/user/tchelvis.service"),
            paths
                .home_dir
                .join(format!(".claude/skills/{ORCH_SKILL_NAME}/SKILL.md")),
            paths.home_dir.join(format!(
                ".config/opencode/skills/{ORCH_SKILL_NAME}/SKILL.md"
            )),
            paths
                .home_dir
                .join(format!(".config/opencode/commands/{ORCH_SKILL_NAME}.md")),
            paths
                .home_dir
                .join(format!(".pi/agent/skills/{ORCH_SKILL_NAME}/SKILL.md")),
            paths
                .home_dir
                .join(format!(".agents/skills/{ORCH_SKILL_NAME}/SKILL.md")),
            paths
                .home_dir
                .join(format!(".codex/skills/{ORCH_SKILL_NAME}/SKILL.md")),
            paths
                .home_dir
                .join(format!(".gemini/skills/{ORCH_SKILL_NAME}/SKILL.md")),
        ] {
            if extra_file.exists() {
                actions.push(format!("remove file {}", extra_file.display()));
                if !dry_run {
                    let _ = fs::remove_file(&extra_file);
                }
            }
        }

        let to_tickets_dir = paths
            .home_dir
            .join(".agents/skills")
            .join(TO_TICKETS_SKILL_NAME);
        let to_tickets_file = to_tickets_dir.join("SKILL.md");
        if !is_symlink_path(&paths.home_dir.join(".agents"))
            && !is_symlink_path(&paths.home_dir.join(".agents/skills"))
            && !is_symlink_path(&to_tickets_dir)
        {
            if let Ok(meta) = fs::symlink_metadata(&to_tickets_file) {
                if meta.file_type().is_file()
                    && fs::read(&to_tickets_file).ok().as_deref()
                        == Some(TO_TICKETS_SKILL.as_bytes())
                {
                    actions.push(format!("remove file {}", to_tickets_file.display()));
                    if !dry_run {
                        let _ = fs::remove_file(&to_tickets_file);
                    }
                }
            }
        }

        let to_tickets_link = paths
            .home_dir
            .join(".claude/skills")
            .join(TO_TICKETS_SKILL_NAME);
        if !is_symlink_path(&paths.home_dir.join(".claude"))
            && !is_symlink_path(&paths.home_dir.join(".claude/skills"))
        {
            if let Ok(meta) = fs::symlink_metadata(&to_tickets_link) {
                if meta.file_type().is_symlink()
                    && fs::read_link(&to_tickets_link).ok().as_ref() == Some(&to_tickets_dir)
                {
                    actions.push(format!("remove file {}", to_tickets_link.display()));
                    if !dry_run {
                        let _ = fs::remove_file(&to_tickets_link);
                    }
                }
            }
        }

        let herdr_mod_dir = paths.home_dir.join(".config/herdr-app/mods/tchelvis");
        if herdr_mod_dir.exists() {
            actions.push(format!("remove directory {}", herdr_mod_dir.display()));
            if !dry_run {
                let _ = fs::remove_dir_all(&herdr_mod_dir);
            }
        }

        cleanup_legacy_supervisor_mods(&paths.home_dir, dry_run, &mut actions);

        for dir in [
            &paths.runtime_dir,
            &paths.data_dir,
            &paths.state_dir,
            &paths.config_dir,
        ] {
            if dir.exists() {
                actions.push(format!("purge directory {}", dir.display()));
                if !dry_run {
                    let _ = fs::remove_dir_all(dir);
                }
            }
        }

        let bin_to_remove = custom_binary_path
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok());
        if let Some(bin_path) = bin_to_remove {
            let bin_str = bin_path.to_string_lossy();
            // Guard against unlinking cargo test binaries during test runs unless explicitly provided via custom_binary_path
            let is_cargo_test_bin = custom_binary_path.is_none()
                && (bin_str.contains("/target/debug/deps/")
                    || bin_str.contains("/target/release/deps/"));
            if !is_cargo_test_bin && bin_path.exists() {
                actions.push(format!("remove binary {}", bin_path.display()));
                if !dry_run {
                    let _ = fs::remove_file(&bin_path);
                }
            }
        }
    }

    Ok(SetupReport {
        dry_run,
        action: if purge {
            "uninstall --purge".into()
        } else {
            "uninstall".into()
        },
        planned_or_applied: actions,
        verified_ok: true,
    })
}

pub fn verify_installation(paths: &CockpitPaths) -> Result<SetupReport> {
    let mut checks = Vec::new();
    let mut all_ok = true;

    let hooks_dir = paths.hooks_dir();
    for (name, expected_content) in [("shell.sh", SHELL_SH), ("claude-hook.sh", CLAUDE_HOOK_SH)] {
        let p = hooks_dir.join(name);
        if p.exists() {
            let actual = fs::read_to_string(&p).unwrap_or_default();
            if asset_sha256(&actual) == asset_sha256(expected_content) {
                checks.push(format!("OK: {} matches embedded SHA-256", p.display()));
            } else {
                all_ok = false;
                checks.push(format!(
                    "MISMATCH: {} differs from binary asset",
                    p.display()
                ));
            }
        } else {
            all_ok = false;
            checks.push(format!("MISSING: {}", p.display()));
        }
    }

    for (rel_path, expected_content) in [
        (".config/herdr-app/mods/tchelvis/main.js", HERDR_MOD_MAIN_JS),
        (".config/herdr-app/mods/tchelvis/mod.json", HERDR_MOD_JSON),
        (".config/herdr-app/layouts/tchelvis.json", HERDR_LAYOUT_JSON),
    ] {
        let p = paths.home_dir.join(rel_path);
        if p.exists() {
            let actual = fs::read_to_string(&p).unwrap_or_default();
            if asset_sha256(&actual) == asset_sha256(expected_content) {
                checks.push(format!("OK: {} matches embedded SHA-256", p.display()));
            } else {
                all_ok = false;
                checks.push(format!("MISMATCH: {}", p.display()));
            }
        } else {
            all_ok = false;
            checks.push(format!("MISSING: {}", p.display()));
        }
    }

    let icons_dir = paths.home_dir.join(".config/herdr-app/mods/tchelvis/icons");
    for (file_name, expected_content) in HERDR_MOD_ICONS {
        let p = icons_dir.join(file_name);
        if p.exists() {
            let actual = fs::read_to_string(&p).unwrap_or_default();
            if asset_sha256(&actual) == asset_sha256(expected_content) {
                checks.push(format!("OK: {} matches embedded SHA-256", p.display()));
            } else {
                all_ok = false;
                checks.push(format!("MISMATCH: {}", p.display()));
            }
        } else {
            all_ok = false;
            checks.push(format!("MISSING: {}", p.display()));
        }
    }

    Ok(SetupReport {
        dry_run: false,
        action: "verify".into(),
        planned_or_applied: checks,
        verified_ok: all_ok,
    })
}

fn cleanup_legacy_supervisor_mods(home_dir: &Path, dry_run: bool, actions: &mut Vec<String>) {
    let herdr_dir = home_dir.join(".config/herdr-app");
    for mod_name in [
        "supervisor",
        "supervisor-pill",
        "supervisor-fleet",
        "supervisor-inspector",
    ] {
        let dir = herdr_dir.join("mods").join(mod_name);
        if dir.exists() {
            actions.push(format!("remove legacy mod directory {}", dir.display()));
            if !dry_run {
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }
    let legacy_layout = herdr_dir.join("layouts/supervisor.json");
    if legacy_layout.exists() {
        actions.push(format!("remove legacy layout {}", legacy_layout.display()));
        if !dry_run {
            let _ = fs::remove_file(&legacy_layout);
        }
    }
}

fn is_symlink_path(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn apply_to_tickets_skill(
    paths: &CockpitPaths,
    dry_run: bool,
    manifest: &mut SetupManifest,
    actions: &mut Vec<String>,
    confirm: &mut dyn FnMut(&Path) -> bool,
) -> Result<()> {
    let agents_dir = paths.home_dir.join(".agents");
    let skills_dir = agents_dir.join("skills");
    let skill_dir = skills_dir.join(TO_TICKETS_SKILL_NAME);
    let skill_path = skill_dir.join("SKILL.md");

    if is_symlink_path(&agents_dir)
        || is_symlink_path(&skills_dir)
        || is_symlink_path(&skill_dir)
        || is_symlink_path(&skill_path)
    {
        actions.push(format!(
            "skip {} (symlink; not managed by tchelvis)",
            skill_path.display()
        ));
        return Ok(());
    }

    let ours_hash = asset_sha256(TO_TICKETS_SKILL);
    let skill_path_str = skill_path.to_string_lossy().to_string();

    let file_is_ours = match fs::symlink_metadata(&skill_path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            actions.push(format!("write {}", skill_path.display()));
            if !dry_run {
                atomic_write_file(&skill_path, TO_TICKETS_SKILL, 0o644)?;
                upsert_manifest_entry(
                    manifest,
                    ManifestEntry {
                        path: skill_path_str.clone(),
                        existed_before: false,
                        pre_sha256: None,
                        post_sha256: ours_hash,
                        action: ManifestActionKind::CreatedFile,
                        keep_if_modified: true,
                    },
                );
            }
            true
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed inspecting {}", skill_path.display()))
        }
        Ok(meta) => {
            if !meta.file_type().is_file() {
                actions.push(format!(
                    "skip {} (not a regular file; not managed by tchelvis)",
                    skill_path.display()
                ));
                return Ok(());
            }
            let bytes = fs::read(&skill_path)
                .with_context(|| format!("failed reading {}", skill_path.display()))?;
            let actual_hash = bytes_sha256(&bytes);

            if actual_hash == ours_hash {
                actions.push(format!("up-to-date {}", skill_path.display()));
                // Also refreshes a hash left behind by an interrupted setup.
                if !dry_run {
                    upsert_manifest_entry(
                        manifest,
                        ManifestEntry {
                            path: skill_path_str.clone(),
                            existed_before: true,
                            pre_sha256: Some(actual_hash),
                            post_sha256: ours_hash,
                            action: ManifestActionKind::CreatedFile,
                            keep_if_modified: true,
                        },
                    );
                }
                true
            } else if manifest
                .entries
                .iter()
                .any(|e| e.path == skill_path_str && e.post_sha256 == actual_hash)
            {
                actions.push(format!("write {}", skill_path.display()));
                if !dry_run {
                    atomic_write_file(&skill_path, TO_TICKETS_SKILL, 0o644)?;
                    upsert_manifest_entry(
                        manifest,
                        ManifestEntry {
                            path: skill_path_str.clone(),
                            existed_before: true,
                            pre_sha256: Some(actual_hash),
                            post_sha256: ours_hash,
                            action: ManifestActionKind::CreatedFile,
                            keep_if_modified: true,
                        },
                    );
                }
                true
            } else if dry_run {
                actions.push(format!(
                    "would ask before replacing {}",
                    skill_path.display()
                ));
                false
            } else if confirm(&skill_path) {
                actions.push(format!("write {}", skill_path.display()));
                atomic_write_file(&skill_path, TO_TICKETS_SKILL, 0o644)?;
                upsert_manifest_entry(
                    manifest,
                    ManifestEntry {
                        path: skill_path_str.clone(),
                        existed_before: true,
                        pre_sha256: Some(actual_hash),
                        post_sha256: ours_hash,
                        action: ManifestActionKind::CreatedFile,
                        keep_if_modified: true,
                    },
                );
                true
            } else {
                actions.push(format!(
                    "skip {} (your own file; not replaced)",
                    skill_path.display()
                ));
                false
            }
        }
    };

    let claude_dir = paths.home_dir.join(".claude");
    let claude_exists = fs::symlink_metadata(&claude_dir)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false);
    if claude_exists && file_is_ours {
        let claude_skills_dir = claude_dir.join("skills");
        if !is_symlink_path(&claude_skills_dir) {
            let link_path = claude_skills_dir.join(TO_TICKETS_SKILL_NAME);
            if matches!(
                fs::symlink_metadata(&link_path),
                Err(ref e) if e.kind() == std::io::ErrorKind::NotFound
            ) {
                actions.push(format!(
                    "symlink {} -> {}",
                    link_path.display(),
                    skill_dir.display()
                ));
                if !dry_run {
                    fs::create_dir_all(&claude_skills_dir)?;
                    std::os::unix::fs::symlink(&skill_dir, &link_path)?;
                    let target_sha = asset_sha256(&skill_dir.to_string_lossy());
                    upsert_manifest_entry(
                        manifest,
                        ManifestEntry {
                            path: link_path.to_string_lossy().to_string(),
                            existed_before: false,
                            pre_sha256: None,
                            post_sha256: target_sha,
                            action: ManifestActionKind::CreatedFile,
                            keep_if_modified: true,
                        },
                    );
                }
            }
        }
    }

    Ok(())
}

fn apply_created_file(
    path: &Path,
    content: &str,
    mode: u32,
    dry_run: bool,
    manifest: &mut SetupManifest,
    actions: &mut Vec<String>,
) -> Result<()> {
    let existed = path.exists();
    let pre_hash = if existed {
        fs::read_to_string(path).ok().map(|s| asset_sha256(&s))
    } else {
        None
    };
    let post_hash = asset_sha256(content);

    if pre_hash.as_deref() == Some(post_hash.as_str()) {
        actions.push(format!("up-to-date {}", path.display()));
        return Ok(());
    }

    actions.push(format!("write {}", path.display()));
    if !dry_run {
        atomic_write_file(path, content, mode)?;
        upsert_manifest_entry(
            manifest,
            ManifestEntry {
                path: path.to_string_lossy().to_string(),
                existed_before: existed,
                pre_sha256: pre_hash,
                post_sha256: post_hash,
                action: ManifestActionKind::CreatedFile,
                keep_if_modified: false,
            },
        );
    }
    Ok(())
}

fn apply_fenced_block(
    path: &Path,
    block: &str,
    dry_run: bool,
    manifest: &mut SetupManifest,
    actions: &mut Vec<String>,
) -> Result<()> {
    let existed = path.exists();
    let raw = if existed {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    let pre_hash = if existed {
        Some(asset_sha256(&raw))
    } else {
        None
    };

    let stripped = strip_fenced_block(&raw);
    let mut updated = stripped.trim_end().to_string();
    if !updated.is_empty() {
        updated.push_str("\n\n");
    }
    updated.push_str(block);
    updated.push('\n');

    let post_hash = asset_sha256(&updated);
    if pre_hash.as_deref() == Some(post_hash.as_str()) {
        actions.push(format!("up-to-date fenced block in {}", path.display()));
        return Ok(());
    }

    actions.push(format!("update fenced block in {}", path.display()));
    if !dry_run {
        atomic_write_file(path, &updated, 0o644)?;
        upsert_manifest_entry(
            manifest,
            ManifestEntry {
                path: path.to_string_lossy().to_string(),
                existed_before: existed,
                pre_sha256: pre_hash,
                post_sha256: post_hash,
                action: ManifestActionKind::FencedBlock,
                keep_if_modified: false,
            },
        );
    }
    Ok(())
}

pub fn strip_fenced_block(input: &str) -> String {
    let mut out = Vec::new();
    let mut inside = false;
    for line in input.lines() {
        if line.trim() == FENCE_START {
            inside = true;
            continue;
        }
        if line.trim() == FENCE_END {
            inside = false;
            continue;
        }
        if !inside {
            out.push(line);
        }
    }
    let mut res = out.join("\n").trim_end().to_string();
    if input.ends_with('\n') && !res.is_empty() {
        res.push('\n');
    }
    res
}

fn apply_claude_settings_merge(
    path: &Path,
    hook_cmd: &str,
    dry_run: bool,
    manifest: &mut SetupManifest,
    actions: &mut Vec<String>,
) -> Result<()> {
    let existed = path.exists();
    let (pre_hash, prev_val, mut root): (Option<String>, Option<Value>, Value) = if existed {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading {}", path.display()))?;
        let val: Value = serde_json::from_str(&raw)
            .with_context(|| format!("refusing to modify malformed JSON in {}", path.display()))?;
        (Some(asset_sha256(&raw)), Some(val.clone()), val)
    } else {
        (None, None, json!({}))
    };

    let obj = root
        .as_object_mut()
        .context("Claude settings.json root must be a JSON object")?;
    let hooks_val = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj = hooks_val
        .as_object_mut()
        .context("Claude settings.json 'hooks' must be a JSON object")?;

    for event_name in [
        "UserPromptSubmit",
        "PermissionRequest",
        "Notification",
        "Stop",
    ] {
        let arr_val = hooks_obj.entry(event_name).or_insert_with(|| json!([]));
        if let Some(arr) = arr_val.as_array_mut() {
            // Remove existing tchelvis-managed entry to remain idempotent
            arr.retain(|item| !item.to_string().contains(CLAUDE_HOOK_MARKER));
            arr.push(json!({
                "matcher": "",
                "hooks": [
                    {
                        "type": "command",
                        "command": hook_cmd
                    }
                ]
            }));
        }
    }

    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    let post_hash = asset_sha256(&formatted);
    if pre_hash.as_deref() == Some(post_hash.as_str()) {
        actions.push(format!("up-to-date Claude hooks in {}", path.display()));
        return Ok(());
    }

    actions.push(format!("merge Claude hooks in {}", path.display()));
    if !dry_run {
        atomic_write_file(path, &formatted, 0o644)?;
        upsert_manifest_entry(
            manifest,
            ManifestEntry {
                path: path.to_string_lossy().to_string(),
                existed_before: existed,
                pre_sha256: pre_hash,
                post_sha256: post_hash,
                action: ManifestActionKind::JsonMerge {
                    previous_value: prev_val,
                },
                keep_if_modified: false,
            },
        );
    }
    Ok(())
}

fn revert_claude_hooks_surgically(path: &Path) -> Result<()> {
    let raw = fs::read_to_string(path)?;
    let mut root: Value = serde_json::from_str(&raw)?;
    if let Some(hooks_obj) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_k, arr_val) in hooks_obj.iter_mut() {
            if let Some(arr) = arr_val.as_array_mut() {
                arr.retain(|item| !item.to_string().contains(CLAUDE_HOOK_MARKER));
            }
        }
    }
    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write_file(path, &formatted, 0o644)
}

fn apply_herdr_default_layout(
    path: &Path,
    dry_run: bool,
    manifest: &mut SetupManifest,
    actions: &mut Vec<String>,
) -> Result<()> {
    let existed = path.exists();
    let (pre_hash, prev_val, mut root): (Option<String>, Option<Value>, Value) = if existed {
        let raw = fs::read_to_string(path)?;
        let val: Value = serde_json::from_str(&raw)
            .with_context(|| format!("malformed JSON in {}", path.display()))?;
        (Some(asset_sha256(&raw)), Some(val.clone()), val)
    } else {
        (None, None, json!({}))
    };

    if let Some(obj) = root.as_object_mut() {
        obj.insert("default_layout".to_string(), json!("tchelvis"));
    }

    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    let post_hash = asset_sha256(&formatted);
    actions.push(format!("set default_layout=tchelvis in {}", path.display()));
    if !dry_run {
        atomic_write_file(path, &formatted, 0o644)?;
        upsert_manifest_entry(
            manifest,
            ManifestEntry {
                path: path.to_string_lossy().to_string(),
                existed_before: existed,
                pre_sha256: pre_hash,
                post_sha256: post_hash,
                action: ManifestActionKind::JsonMerge {
                    previous_value: prev_val,
                },
                keep_if_modified: false,
            },
        );
    }
    Ok(())
}

fn apply_herdr_shared_layout(
    path: &Path,
    dry_run: bool,
    manifest: &mut SetupManifest,
    actions: &mut Vec<String>,
) -> Result<()> {
    let existed = path.exists();
    let (pre_hash, prev_val, mut root): (Option<String>, Option<Value>, Value) = if existed {
        let raw = fs::read_to_string(path)?;
        let val: Value = serde_json::from_str(&raw)
            .with_context(|| format!("malformed JSON in {}", path.display()))?;
        (Some(asset_sha256(&raw)), Some(val.clone()), val)
    } else {
        (None, None, json!({}))
    };

    if let Some(obj) = root.as_object_mut() {
        obj.insert("layout".to_string(), json!("tchelvis"));
    }

    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    let post_hash = asset_sha256(&formatted);
    actions.push(format!("set layout=tchelvis in {}", path.display()));
    if !dry_run {
        atomic_write_file(path, &formatted, 0o644)?;
        upsert_manifest_entry(
            manifest,
            ManifestEntry {
                path: path.to_string_lossy().to_string(),
                existed_before: existed,
                pre_sha256: pre_hash,
                post_sha256: post_hash,
                action: ManifestActionKind::JsonMerge {
                    previous_value: prev_val,
                },
                keep_if_modified: false,
            },
        );
    }
    Ok(())
}

fn upsert_manifest_entry(manifest: &mut SetupManifest, entry: ManifestEntry) {
    if let Some(existing) = manifest.entries.iter_mut().find(|e| e.path == entry.path) {
        // Keep the original pre_sha256 and existed_before from the first time Cockpit touched it
        existing.post_sha256 = entry.post_sha256;
        existing.keep_if_modified = entry.keep_if_modified;
    } else {
        manifest.entries.push(entry);
    }
}

pub fn load_manifest(paths: &CockpitPaths) -> Result<Option<SetupManifest>> {
    let p = paths.setup_manifest_file();
    if !p.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&p)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

pub fn save_manifest(paths: &CockpitPaths, manifest: &SetupManifest) -> Result<()> {
    let p = paths.setup_manifest_file();
    let formatted = serde_json::to_string_pretty(manifest)? + "\n";
    atomic_write_file(&p, &formatted, 0o600)
}

pub fn atomic_write_file(path: &Path, content: &str, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension(format!("tchelvis-tmp-{}", std::process::id()));
    {
        let mut f = File::create(&tmp_path)
            .with_context(|| format!("failed creating temp file {}", tmp_path.display()))?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed renaming {} to {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_and_uninstall_restores_pre_existing_files_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = CockpitPaths {
            home_dir: home.to_path_buf(),
            config_dir: home.join(".config/tchelvis"),
            data_dir: home.join(".local/share/tchelvis"),
            state_dir: home.join(".local/state/tchelvis"),
            runtime_dir: home.join(".local/state/tchelvis/run"),
        };

        let bashrc = home.join(".bashrc");
        let original_bashrc = "export PATH=\"$HOME/.local/bin:$PATH\"\n";
        fs::write(&bashrc, original_bashrc).unwrap();

        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        let claude_settings = claude_dir.join("settings.json");
        let original_claude = "{\n  \"theme\": \"dark\",\n  \"hooks\": {\n    \"Stop\": [\n      {\n        \"matcher\": \"\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"echo user-stop\"\n          }\n        ]\n      }\n    ]\n  }\n}\n";
        fs::write(&claude_settings, original_claude).unwrap();

        // Run setup
        let opts = SetupOptions::default();
        let rep = run_setup(&paths, &opts).unwrap();
        assert!(rep.verified_ok);

        // Run setup second time (idempotency check)
        let after_first = fs::read_to_string(&claude_settings).unwrap();
        run_setup(&paths, &opts).unwrap();
        let after_second = fs::read_to_string(&claude_settings).unwrap();
        assert_eq!(after_first, after_second);

        // Verify installation
        let v_rep = verify_installation(&paths).unwrap();
        assert!(v_rep.verified_ok);

        // Run uninstall
        run_uninstall(&paths, false, false, None).unwrap();
        assert_eq!(fs::read_to_string(&bashrc).unwrap(), original_bashrc);
        assert_eq!(
            fs::read_to_string(&claude_settings).unwrap(),
            original_claude
        );
        assert!(!home
            .join(".config/herdr-app/mods/tchelvis/main.js")
            .exists());
        assert!(!home
            .join(".config/herdr-app/mods/tchelvis/mod.json")
            .exists());

        // Run uninstall --purge even after setup-manifest.json was already deleted
        let dummy_bin = home.join(".local/bin/tchelvis");
        fs::create_dir_all(dummy_bin.parent().unwrap()).unwrap();
        fs::write(&dummy_bin, "binary").unwrap();
        paths.ensure_dirs().unwrap();
        let purge_rep = run_uninstall(&paths, false, true, Some(&dummy_bin)).unwrap();
        assert_eq!(purge_rep.action, "uninstall --purge");
        assert!(!paths.config_dir.exists());
        assert!(!paths.data_dir.exists());
        assert!(!paths.state_dir.exists());
        assert!(!dummy_bin.exists());
    }

    #[test]
    fn default_targets_include_skills_and_skills_is_a_valid_target() {
        let targets = resolve_active_targets(&SetupOptions::default()).unwrap();
        assert!(targets.contains("skills"));

        let only_skills = resolve_active_targets(&SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(only_skills.into_iter().collect::<Vec<_>>(), vec!["skills"]);
    }

    #[test]
    fn setup_installs_orchestration_skill_and_uninstall_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = CockpitPaths {
            home_dir: home.to_path_buf(),
            config_dir: home.join(".config/tchelvis"),
            data_dir: home.join(".local/share/tchelvis"),
            state_dir: home.join(".local/state/tchelvis"),
            runtime_dir: home.join(".local/state/tchelvis/run"),
        };

        for agent_dir in [
            ".claude",
            ".config/opencode",
            ".pi/agent",
            ".agents",
            ".codex",
            ".gemini",
        ] {
            fs::create_dir_all(home.join(agent_dir)).unwrap();
        }

        run_setup(&paths, &SetupOptions::default()).unwrap();

        let skill_paths = [
            ".claude/skills/orchestration-tchelvis/SKILL.md",
            ".config/opencode/skills/orchestration-tchelvis/SKILL.md",
            ".pi/agent/skills/orchestration-tchelvis/SKILL.md",
            ".agents/skills/orchestration-tchelvis/SKILL.md",
            ".codex/skills/orchestration-tchelvis/SKILL.md",
            ".gemini/skills/orchestration-tchelvis/SKILL.md",
        ];
        let expected_skill = crate::orch::preamble::orchestration_skill_file_markdown();
        for rel in skill_paths {
            let installed =
                fs::read_to_string(home.join(rel)).unwrap_or_else(|e| panic!("missing {rel}: {e}"));
            assert_eq!(installed, expected_skill, "unexpected content for {rel}");
        }

        let command_path = home.join(".config/opencode/commands/orchestration-tchelvis.md");
        assert_eq!(
            fs::read_to_string(&command_path).unwrap(),
            crate::orch::preamble::orchestration_opencode_command_markdown()
        );

        let manifest = load_manifest(&paths).unwrap().expect("setup manifest");
        for rel in skill_paths {
            let expected = home.join(rel).to_string_lossy().to_string();
            assert!(
                manifest.entries.iter().any(|e| e.path == expected),
                "manifest missing {rel}"
            );
        }
        assert!(manifest
            .entries
            .iter()
            .any(|e| e.path == command_path.to_string_lossy()));

        run_uninstall(&paths, false, false, None).unwrap();

        for rel in skill_paths {
            assert!(!home.join(rel).exists(), "{rel} was not removed");
        }
        assert!(!command_path.exists(), "opencode command was not removed");
    }

    #[test]
    fn setup_installs_every_mod_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = CockpitPaths {
            home_dir: home.to_path_buf(),
            config_dir: home.join(".config/tchelvis"),
            data_dir: home.join(".local/share/tchelvis"),
            state_dir: home.join(".local/state/tchelvis"),
            runtime_dir: home.join(".local/state/tchelvis/run"),
        };

        let opts = SetupOptions {
            only: Some("herdr-mod".into()),
            ..Default::default()
        };
        let rep = run_setup(&paths, &opts).unwrap();
        assert!(rep.verified_ok);

        let icons_dir = home.join(".config/herdr-app/mods/tchelvis/icons");
        for (name, content) in crate::assets::HERDR_MOD_ICONS {
            let installed = fs::read_to_string(icons_dir.join(name)).unwrap();
            assert_eq!(installed, *content, "mismatch for icon {name}");
        }

        let v_rep = verify_installation(&paths).unwrap();
        for (name, _) in crate::assets::HERDR_MOD_ICONS {
            let expected_path = icons_dir.join(name).display().to_string();
            assert!(
                v_rep
                    .planned_or_applied
                    .iter()
                    .any(|line| line.starts_with("OK:") && line.contains(&expected_path)),
                "missing OK verification line for {expected_path}"
            );
        }
    }

    fn test_paths(home: &Path) -> CockpitPaths {
        CockpitPaths {
            home_dir: home.to_path_buf(),
            config_dir: home.join(".config/tchelvis"),
            data_dir: home.join(".local/share/tchelvis"),
            state_dir: home.join(".local/state/tchelvis"),
            runtime_dir: home.join(".local/state/tchelvis/run"),
        }
    }

    fn contains_named_entry(root: &Path, target_name: &str) -> bool {
        if !root.exists() {
            return false;
        }
        let Ok(entries) = fs::read_dir(root) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(target_name) {
                return true;
            }
            if path.is_dir() && contains_named_entry(&path, target_name) {
                return true;
            }
        }
        false
    }

    #[test]
    fn setup_installs_to_tickets_only_in_agents_and_uninstall_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = test_paths(home);

        for dir in [
            ".claude",
            ".config/opencode",
            ".pi/agent",
            ".codex",
            ".gemini",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }

        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };
        run_setup(&paths, &opts).unwrap();

        let skill_file = home.join(".agents/skills/to-tickets/SKILL.md");
        assert_eq!(
            fs::read_to_string(&skill_file).unwrap(),
            crate::assets::TO_TICKETS_SKILL
        );
        let claude_link = home.join(".claude/skills/to-tickets");
        let link_meta = fs::symlink_metadata(&claude_link).unwrap();
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(
            fs::read_link(&claude_link).unwrap(),
            home.join(".agents/skills/to-tickets")
        );

        for dir in [".config/opencode", ".pi", ".codex", ".gemini"] {
            assert!(
                !contains_named_entry(&home.join(dir), "to-tickets"),
                "unexpected to-tickets under {dir}"
            );
        }

        run_uninstall(&paths, false, false, None).unwrap();
        assert!(fs::symlink_metadata(&skill_file).is_err());
        assert!(fs::symlink_metadata(&claude_link).is_err());
    }

    #[test]
    fn setup_asks_before_replacing_a_foreign_to_tickets() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = test_paths(home);
        fs::create_dir_all(home.join(".claude")).unwrap();

        let skill_file = home.join(".agents/skills/to-tickets/SKILL.md");
        fs::create_dir_all(skill_file.parent().unwrap()).unwrap();
        fs::write(&skill_file, "# mine\n").unwrap();

        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };

        let mut asked = false;
        let rep_no = run_setup_with(&paths, &opts, &mut |p| {
            assert_eq!(p, skill_file);
            asked = true;
            false
        })
        .unwrap();
        assert!(asked);
        assert_eq!(fs::read_to_string(&skill_file).unwrap(), "# mine\n");
        assert!(rep_no
            .planned_or_applied
            .iter()
            .any(|l| l.contains("your own file; not replaced")));
        assert!(fs::symlink_metadata(home.join(".claude/skills/to-tickets")).is_err());
        let manifest = load_manifest(&paths).unwrap().unwrap();
        assert!(!manifest
            .entries
            .iter()
            .any(|e| e.path == skill_file.to_string_lossy()));

        run_uninstall(&paths, false, false, None).unwrap();
        assert_eq!(fs::read_to_string(&skill_file).unwrap(), "# mine\n");

        let mut asked_yes = false;
        run_setup_with(&paths, &opts, &mut |_| {
            asked_yes = true;
            true
        })
        .unwrap();
        assert!(asked_yes);
        assert_eq!(
            fs::read_to_string(&skill_file).unwrap(),
            crate::assets::TO_TICKETS_SKILL
        );
        let manifest_yes = load_manifest(&paths).unwrap().unwrap();
        assert!(manifest_yes
            .entries
            .iter()
            .any(|e| e.path == skill_file.to_string_lossy() && e.keep_if_modified));
        assert!(fs::symlink_metadata(home.join(".claude/skills/to-tickets"))
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn setup_updates_its_own_older_to_tickets_without_asking() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = test_paths(home);
        paths.ensure_dirs().unwrap();

        let skill_file = home.join(".agents/skills/to-tickets/SKILL.md");
        fs::create_dir_all(skill_file.parent().unwrap()).unwrap();
        let older = "# older tchelvis version\n";
        fs::write(&skill_file, older).unwrap();

        let manifest = SetupManifest {
            version: 1,
            updated_at: 0,
            entries: vec![ManifestEntry {
                path: skill_file.to_string_lossy().to_string(),
                existed_before: false,
                pre_sha256: None,
                post_sha256: asset_sha256(older),
                action: ManifestActionKind::CreatedFile,
                keep_if_modified: true,
            }],
        };
        save_manifest(&paths, &manifest).unwrap();

        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };
        run_setup_with(&paths, &opts, &mut |_| {
            panic!("confirm must not be called when updating our own older file")
        })
        .unwrap();

        assert_eq!(
            fs::read_to_string(&skill_file).unwrap(),
            crate::assets::TO_TICKETS_SKILL
        );
        let updated_manifest = load_manifest(&paths).unwrap().unwrap();
        let entry = updated_manifest
            .entries
            .iter()
            .find(|e| e.path == skill_file.to_string_lossy())
            .unwrap();
        assert_eq!(
            entry.post_sha256,
            asset_sha256(crate::assets::TO_TICKETS_SKILL)
        );
    }

    #[test]
    fn an_edited_to_tickets_asks_on_setup_and_survives_uninstall() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = test_paths(home);
        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };
        run_setup(&paths, &opts).unwrap();

        let skill_file = home.join(".agents/skills/to-tickets/SKILL.md");
        let edited = "# edited by user\n";
        fs::write(&skill_file, edited).unwrap();

        let mut asked = false;
        let rep = run_setup_with(&paths, &opts, &mut |_| {
            asked = true;
            false
        })
        .unwrap();
        assert!(asked);
        assert_eq!(fs::read_to_string(&skill_file).unwrap(), edited);
        assert!(rep
            .planned_or_applied
            .iter()
            .any(|l| l.contains("your own file; not replaced")));

        let un_rep = run_uninstall(&paths, false, false, None).unwrap();
        assert_eq!(fs::read_to_string(&skill_file).unwrap(), edited);
        assert!(un_rep
            .planned_or_applied
            .iter()
            .any(|l| l.contains("keep ") && l.contains("(modified)")));
    }

    #[test]
    fn a_symlinked_to_tickets_is_left_alone() {
        use std::os::unix::fs::symlink;

        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };

        // 1. SKILL.md is a symlink
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let target = home.join("external-skill.md");
            fs::write(&target, "# external\n").unwrap();
            let skill_dir = home.join(".agents/skills/to-tickets");
            fs::create_dir_all(&skill_dir).unwrap();
            let skill_file = skill_dir.join("SKILL.md");
            symlink(&target, &skill_file).unwrap();

            let rep = run_setup_with(&paths, &opts, &mut |_| {
                panic!("confirm must not be called for symlink")
            })
            .unwrap();
            assert!(fs::symlink_metadata(&skill_file)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read_to_string(&target).unwrap(), "# external\n");
            assert!(rep
                .planned_or_applied
                .iter()
                .any(|l| l.contains("symlink; not managed by tchelvis")));
        }

        // 2. folder is a symlink
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let target_dir = home.join("external-dir");
            fs::create_dir_all(&target_dir).unwrap();
            let target_file = target_dir.join("SKILL.md");
            fs::write(&target_file, "# external dir\n").unwrap();
            let skills_root = home.join(".agents/skills");
            fs::create_dir_all(&skills_root).unwrap();
            let skill_dir = skills_root.join("to-tickets");
            symlink(&target_dir, &skill_dir).unwrap();

            let rep = run_setup_with(&paths, &opts, &mut |_| {
                panic!("confirm must not be called for folder symlink")
            })
            .unwrap();
            assert!(fs::symlink_metadata(&skill_dir)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                fs::read_to_string(&target_file).unwrap(),
                "# external dir\n"
            );
            assert!(rep
                .planned_or_applied
                .iter()
                .any(|l| l.contains("symlink; not managed by tchelvis")));
        }

        // 3. dangling folder symlink
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let missing_dir = home.join("does-not-exist");
            let skills_root = home.join(".agents/skills");
            fs::create_dir_all(&skills_root).unwrap();
            let skill_dir = skills_root.join("to-tickets");
            symlink(&missing_dir, &skill_dir).unwrap();

            let rep = run_setup_with(&paths, &opts, &mut |_| {
                panic!("confirm must not be called for dangling symlink")
            })
            .unwrap();
            assert!(fs::symlink_metadata(&skill_dir)
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(!missing_dir.exists());
            assert!(rep
                .planned_or_applied
                .iter()
                .any(|l| l.contains("symlink; not managed by tchelvis")));
        }
    }

    #[test]
    fn an_existing_claude_entry_is_left_alone() {
        use std::os::unix::fs::symlink;

        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };

        // 1. Existing directory at ~/.claude/skills/to-tickets
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let claude_entry = home.join(".claude/skills/to-tickets");
            fs::create_dir_all(&claude_entry).unwrap();
            fs::write(claude_entry.join("SKILL.md"), "# user claude dir\n").unwrap();

            run_setup(&paths, &opts).unwrap();
            assert!(claude_entry.is_dir());
            assert_eq!(
                fs::read_to_string(claude_entry.join("SKILL.md")).unwrap(),
                "# user claude dir\n"
            );
        }

        // 2. Existing regular file at ~/.claude/skills/to-tickets
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let claude_entry = home.join(".claude/skills/to-tickets");
            fs::create_dir_all(claude_entry.parent().unwrap()).unwrap();
            fs::write(&claude_entry, "custom file\n").unwrap();

            run_setup(&paths, &opts).unwrap();
            assert!(fs::symlink_metadata(&claude_entry)
                .unwrap()
                .file_type()
                .is_file());
            assert_eq!(fs::read_to_string(&claude_entry).unwrap(), "custom file\n");
        }

        // 3. Existing foreign symlink at ~/.claude/skills/to-tickets
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let foreign_target = home.join("other-skills/to-tickets");
            fs::create_dir_all(&foreign_target).unwrap();
            let claude_entry = home.join(".claude/skills/to-tickets");
            fs::create_dir_all(claude_entry.parent().unwrap()).unwrap();
            symlink(&foreign_target, &claude_entry).unwrap();

            run_setup(&paths, &opts).unwrap();
            assert_eq!(fs::read_link(&claude_entry).unwrap(), foreign_target);
        }
    }

    #[test]
    fn purge_without_a_manifest_removes_only_our_to_tickets() {
        use std::os::unix::fs::symlink;

        // 1. Exact embedded file + our symlink, no manifest -> both removed on purge
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let skill_dir = home.join(".agents/skills/to-tickets");
            fs::create_dir_all(&skill_dir).unwrap();
            let skill_file = skill_dir.join("SKILL.md");
            fs::write(&skill_file, crate::assets::TO_TICKETS_SKILL).unwrap();
            let claude_skills = home.join(".claude/skills");
            fs::create_dir_all(&claude_skills).unwrap();
            let claude_link = claude_skills.join("to-tickets");
            symlink(&skill_dir, &claude_link).unwrap();

            run_uninstall(&paths, false, true, None).unwrap();
            assert!(fs::symlink_metadata(&skill_file).is_err());
            assert!(fs::symlink_metadata(&claude_link).is_err());
        }

        // 2. Foreign file + foreign symlink, no manifest -> both kept on purge
        {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path();
            let paths = test_paths(home);
            let skill_dir = home.join(".agents/skills/to-tickets");
            fs::create_dir_all(&skill_dir).unwrap();
            let skill_file = skill_dir.join("SKILL.md");
            fs::write(&skill_file, "# foreign\n").unwrap();
            let foreign_target = home.join("elsewhere/to-tickets");
            fs::create_dir_all(&foreign_target).unwrap();
            let claude_skills = home.join(".claude/skills");
            fs::create_dir_all(&claude_skills).unwrap();
            let claude_link = claude_skills.join("to-tickets");
            symlink(&foreign_target, &claude_link).unwrap();

            run_uninstall(&paths, false, true, None).unwrap();
            assert_eq!(fs::read_to_string(&skill_file).unwrap(), "# foreign\n");
            assert_eq!(fs::read_link(&claude_link).unwrap(), foreign_target);
        }
    }

    #[test]
    fn a_manifest_without_the_new_field_loads() {
        let json = r#"{
            "version": 1,
            "updated_at": 1700000000,
            "entries": [
                {
                    "path": "/tmp/example",
                    "existed_before": false,
                    "pre_sha256": null,
                    "post_sha256": "abc123",
                    "action": { "kind": "created_file" }
                }
            ]
        }"#;
        let parsed: SetupManifest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert!(!parsed.entries[0].keep_if_modified);
    }

    #[test]
    fn dry_run_writes_no_to_tickets_and_never_asks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let paths = test_paths(home);
        fs::create_dir_all(home.join(".claude")).unwrap();

        let opts = SetupOptions {
            dry_run: true,
            only: Some("skills".into()),
            ..Default::default()
        };

        // Absent case
        run_setup_with(&paths, &opts, &mut |_| {
            panic!("confirm must not be called in dry_run")
        })
        .unwrap();
        let skill_file = home.join(".agents/skills/to-tickets/SKILL.md");
        assert!(fs::symlink_metadata(&skill_file).is_err());
        assert!(fs::symlink_metadata(home.join(".claude/skills/to-tickets")).is_err());

        // Foreign file case
        fs::create_dir_all(skill_file.parent().unwrap()).unwrap();
        fs::write(&skill_file, "# foreign\n").unwrap();
        let rep = run_setup_with(&paths, &opts, &mut |_| {
            panic!("confirm must not be called in dry_run")
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&skill_file).unwrap(), "# foreign\n");
        assert!(rep
            .planned_or_applied
            .iter()
            .any(|l| l.contains("would ask before replacing")));
    }

    #[test]
    fn uninstall_never_follows_a_linked_parent_of_to_tickets() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let paths = test_paths(&home);
        let opts = SetupOptions {
            only: Some("skills".into()),
            ..Default::default()
        };
        run_setup(&paths, &opts).unwrap();

        let shared = tmp.path().join("shared-agents");
        fs::rename(home.join(".agents/skills"), &shared).unwrap();
        std::os::unix::fs::symlink(&shared, home.join(".agents/skills")).unwrap();
        run_uninstall(&paths, false, false, None).unwrap();
        assert_eq!(
            fs::read_to_string(shared.join("to-tickets/SKILL.md")).unwrap(),
            TO_TICKETS_SKILL
        );
    }
}
