use crate::assets::{
    asset_sha256, CLAUDE_HOOK_MARKER, CLAUDE_HOOK_SH, FENCE_END, FENCE_START, HERDR_LAYOUT_JSON,
    HERDR_MOD_JSON, HERDR_MOD_MAIN_JS, OPENCODE_COCKPIT_JS, PI_COCKPIT_JS, SHELL_FISH, SHELL_NU,
    SHELL_SH, SYSTEMD_UNIT_TEMPLATE,
};
use crate::config::CockpitPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const VALID_TARGETS: [&str; 6] = ["shell", "claude", "opencode", "pi", "herdr-mod", "systemd"];

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
        ["shell", "claude", "opencode", "pi", "herdr-mod"]
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
            hooks_dir.join("opencode-cockpit.js"),
            OPENCODE_COCKPIT_JS,
            0o644,
            "opencode",
        ),
        (hooks_dir.join("pi-cockpit.js"), PI_COCKPIT_JS, 0o644, "pi"),
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

    // 3. OpenCode (~/.config/opencode/plugins/cockpit.js)
    if targets.contains("opencode") {
        let opencode_dir = paths.home_dir.join(".config/opencode");
        if opencode_dir.exists() || opts.only.as_deref() == Some("opencode") {
            let plugin_path = opencode_dir.join("plugins/cockpit.js");
            apply_created_file(
                &plugin_path,
                OPENCODE_COCKPIT_JS,
                0o644,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 4. Pi (~/.pi/agent/extensions/cockpit.js)
    if targets.contains("pi") {
        let pi_dir = paths.home_dir.join(".pi/agent");
        if pi_dir.exists() || opts.only.as_deref() == Some("pi") {
            let ext_path = pi_dir.join("extensions/cockpit.js");
            apply_created_file(
                &ext_path,
                PI_COCKPIT_JS,
                0o644,
                opts.dry_run,
                &mut manifest,
                &mut actions,
            )?;
        }
    }

    // 5. Herdr Desktop Mod & Layout
    if targets.contains("herdr-mod") {
        let herdr_dir = paths.home_dir.join(".config/herdr-app");
        let had_herdr_dir = herdr_dir.exists();
        let mod_path = herdr_dir.join("mods/cockpit/main.js");
        let mod_json_path = herdr_dir.join("mods/cockpit/mod.json");
        let layout_path = herdr_dir.join("layouts/cockpit.json");
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
        let unit_path = paths.home_dir.join(".config/systemd/user/cockpit.service");
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
                    if p.exists() {
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
        // Fallback cleanup in case setup-manifest.json was already deleted by an earlier `cockpit uninstall`
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
                .join(".config/herdr-app/layouts/cockpit.json"),
            paths
                .home_dir
                .join(".config/herdr-app/mods/cockpit/main.js"),
            paths
                .home_dir
                .join(".config/herdr-app/mods/cockpit/mod.json"),
            paths.home_dir.join(".pi/agent/extensions/cockpit.js"),
            paths.home_dir.join(".config/opencode/plugins/cockpit.js"),
            paths.home_dir.join(".config/systemd/user/cockpit.service"),
        ] {
            if extra_file.exists() {
                actions.push(format!("remove file {}", extra_file.display()));
                if !dry_run {
                    let _ = fs::remove_file(&extra_file);
                }
            }
        }

        let herdr_mod_dir = paths.home_dir.join(".config/herdr-app/mods/cockpit");
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
        (".config/herdr-app/mods/cockpit/main.js", HERDR_MOD_MAIN_JS),
        (".config/herdr-app/mods/cockpit/mod.json", HERDR_MOD_JSON),
        (".config/herdr-app/layouts/cockpit.json", HERDR_LAYOUT_JSON),
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
            // Remove existing cockpit-managed entry to remain idempotent
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
        obj.insert("default_layout".to_string(), json!("cockpit"));
    }

    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    let post_hash = asset_sha256(&formatted);
    actions.push(format!("set default_layout=cockpit in {}", path.display()));
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
        obj.insert("layout".to_string(), json!("cockpit"));
    }

    let formatted = serde_json::to_string_pretty(&root)? + "\n";
    let post_hash = asset_sha256(&formatted);
    actions.push(format!("set layout=cockpit in {}", path.display()));
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
            },
        );
    }
    Ok(())
}

fn upsert_manifest_entry(manifest: &mut SetupManifest, entry: ManifestEntry) {
    if let Some(existing) = manifest.entries.iter_mut().find(|e| e.path == entry.path) {
        // Keep the original pre_sha256 and existed_before from the first time Cockpit touched it
        existing.post_sha256 = entry.post_sha256;
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
    let tmp_path = path.with_extension(format!("cockpit-tmp-{}", std::process::id()));
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
            config_dir: home.join(".config/cockpit"),
            data_dir: home.join(".local/share/cockpit"),
            state_dir: home.join(".local/state/cockpit"),
            runtime_dir: home.join(".local/state/cockpit/run"),
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
        assert!(!home.join(".config/herdr-app/mods/cockpit/main.js").exists());
        assert!(!home.join(".config/herdr-app/mods/cockpit/mod.json").exists());

        // Run uninstall --purge even after setup-manifest.json was already deleted
        let dummy_bin = home.join(".local/bin/cockpit");
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
}
