use crate::loops::workflow::RoleDefaults;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_MAX_CONCURRENT: u32 = 2;

/// Where tchelvis keeps its own configuration and state: never visible to a stage.
const TCHELVIS_HOME_DIRS: &[&str] = &[
    ".config/tchelvis",
    ".local/share/tchelvis",
    ".local/state/tchelvis",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxConfig {
    Inherited,
    Bwrap { home_ro: Vec<String> },
}

pub const DEFAULT_PREFLIGHT_VALID_HOURS: u32 = 12;
pub const MAX_PREFLIGHT_VALID_HOURS: u32 = 168;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightMode {
    #[default]
    Auto,
    Always,
    Free,
}

impl PreflightMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Free => "free",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightConfig {
    pub mode: PreflightMode,
    pub valid_hours: u32,
}

impl Default for PreflightConfig {
    fn default() -> Self {
        Self {
            mode: PreflightMode::Auto,
            valid_hours: DEFAULT_PREFLIGHT_VALID_HOURS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopsConfig {
    pub max_concurrent: u32,
    pub auto_approve_in_sandbox: bool,
    pub env_pass: Vec<String>,
    pub sandbox: SandboxConfig,
    pub defaults: RoleDefaults,
    pub preflight: PreflightConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLoopsConfig {
    #[serde(default = "default_max_concurrent")]
    max_concurrent: u32,
    #[serde(default)]
    auto_approve_in_sandbox: bool,
    #[serde(default)]
    env_pass: Vec<String>,
    #[serde(default)]
    sandbox: RawSandbox,
    #[serde(default)]
    defaults: RoleDefaults,
    #[serde(default)]
    preflight: RawPreflightConfig,
}

fn default_preflight_valid_hours() -> u32 {
    DEFAULT_PREFLIGHT_VALID_HOURS
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPreflightConfig {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default = "default_preflight_valid_hours")]
    valid_hours: u32,
}

impl Default for RawPreflightConfig {
    fn default() -> Self {
        Self {
            mode: None,
            valid_hours: DEFAULT_PREFLIGHT_VALID_HOURS,
        }
    }
}

fn default_max_concurrent() -> u32 {
    DEFAULT_MAX_CONCURRENT
}

/// The strongest sandbox available today. It is only what an absent `kind` means: a sandbox whose
/// check fails is never replaced by another one.
fn default_sandbox_kind() -> String {
    "bwrap".to_string()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawSandbox {
    kind: String,
    bwrap: Option<RawBwrap>,
    incus: Option<RawIncus>,
}

impl Default for RawSandbox {
    fn default() -> Self {
        Self {
            kind: default_sandbox_kind(),
            bwrap: None,
            incus: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBwrap {
    #[serde(default)]
    home_ro: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIncus {
    #[allow(dead_code)]
    #[serde(default)]
    instance: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    user: Option<String>,
}

fn validate_home_relative_path(entry: &str, field: &str) -> Result<()> {
    if entry.is_empty() || entry.contains('\0') {
        bail!("entrada vazia ou inválida em '{field}'");
    }
    if entry == "~" || entry.starts_with("~/") {
        bail!("entrada '{entry}' em '{field}' deve ser relativa ao HOME, sem '~'");
    }
    let path = Path::new(entry);
    if path.is_absolute() {
        bail!("entrada '{entry}' em '{field}' deve ser um caminho relativo, não absoluto");
    }
    let mut has_normal = false;
    for comp in path.components() {
        match comp {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("entrada '{entry}' em '{field}' não pode escapar do HOME ('..')");
            }
        }
    }
    if !has_normal {
        bail!("entrada '{entry}' em '{field}' deve apontar para um subcaminho válido");
    }
    // `components()` already drops `.` and trailing slashes, so this compares real paths.
    let clean: PathBuf = path
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .collect();
    for own in TCHELVIS_HOME_DIRS {
        let own = Path::new(own);
        if own.starts_with(&clean) || clean.starts_with(own) {
            bail!("entrada '{entry}' em '{field}' exporia os arquivos do tchelvis às etapas");
        }
    }
    Ok(())
}

pub fn parse_loops_config(raw: &str) -> Result<LoopsConfig> {
    let parsed: RawLoopsConfig = toml::from_str(raw).map_err(|e| {
        anyhow::anyhow!(
            "configuração loops.toml inválida ({})",
            crate::loops::workflow::toml_error_reason(raw, &e)
        )
    })?;

    if parsed.max_concurrent == 0 {
        bail!("'max_concurrent' deve ser maior que zero");
    }

    for var in &parsed.env_pass {
        if var.is_empty() || var.contains('=') || var.contains('\0') {
            bail!("nome de variável inválido em 'env_pass': '{var}'");
        }
    }

    for (role, def) in &parsed.defaults {
        if role.is_empty()
            || role.starts_with('.')
            || role.contains('/')
            || role.contains('\\')
            || role.contains("..")
        {
            bail!("nome de role inválido em [defaults.{role}]");
        }
        if let Some(model) = &def.model {
            crate::loops::workflow::validate_model_name(model)
                .map_err(|e| anyhow::anyhow!("[defaults.{role}]: {e}"))?;
        }
    }

    if let Some(bwrap) = &parsed.sandbox.bwrap {
        for p in &bwrap.home_ro {
            validate_home_relative_path(p, "sandbox.bwrap.home_ro")?;
        }
    }

    let sandbox = match parsed.sandbox.kind.as_str() {
        "inherited" => SandboxConfig::Inherited,
        "bwrap" => {
            let b = parsed.sandbox.bwrap.unwrap_or_default();
            SandboxConfig::Bwrap { home_ro: b.home_ro }
        }
        "incus" => {
            let _ = parsed.sandbox.incus;
            bail!("sandbox 'incus' ainda não suportado nesta versão");
        }
        other => bail!("tipo de sandbox desconhecido em [sandbox]: '{other}'"),
    };

    let preflight_mode = match parsed.preflight.mode.as_deref() {
        None | Some("auto") => PreflightMode::Auto,
        Some("always") => PreflightMode::Always,
        Some("free") => PreflightMode::Free,
        Some(other) => {
            bail!("modo desconhecido em [preflight]: '{other}' (use auto, always ou free)")
        }
    };
    if !(1..=MAX_PREFLIGHT_VALID_HOURS).contains(&parsed.preflight.valid_hours) {
        bail!("'valid_hours' em [preflight] deve ficar entre 1 e {MAX_PREFLIGHT_VALID_HOURS}");
    }

    Ok(LoopsConfig {
        max_concurrent: parsed.max_concurrent,
        auto_approve_in_sandbox: parsed.auto_approve_in_sandbox,
        env_pass: parsed.env_pass,
        sandbox,
        defaults: parsed.defaults,
        preflight: PreflightConfig {
            mode: preflight_mode,
            valid_hours: parsed.preflight.valid_hours,
        },
    })
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

pub fn check_home_ro_does_not_reach(
    home: &Path,
    home_ro: &[String],
    protected_paths: &[&Path],
) -> Result<()> {
    let home_norm = std::fs::canonicalize(home).unwrap_or_else(|_| normalize_lexical(home));
    for rel in home_ro {
        let candidate = home.join(rel);
        let cand_norm =
            std::fs::canonicalize(&candidate).unwrap_or_else(|_| normalize_lexical(&candidate));
        if cand_norm == home_norm || !cand_norm.starts_with(&home_norm) {
            bail!("entrada '{rel}' em home_ro não pode apontar para fora ou raiz do HOME");
        }
        for &prot in protected_paths {
            let prot_norm = std::fs::canonicalize(prot).unwrap_or_else(|_| normalize_lexical(prot));
            if prot_norm.starts_with(&cand_norm) || cand_norm.starts_with(&prot_norm) {
                bail!(
                    "entrada '{rel}' em home_ro alcançaria arquivo protegido do tchelvis ({})",
                    prot.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::workflow::Provider;

    #[test]
    fn config_rejects_incus_unknown_keys_and_escaping_home_paths() {
        let valid_inherited = r#"
auto_approve_in_sandbox = true
env_pass = ["ANTHROPIC_API_KEY"]

[sandbox]
kind = "inherited"

[defaults.worker]
provider = "opencode"
model = "zai/glm-5.3-flash"
"#;
        let cfg = parse_loops_config(valid_inherited).expect("valid inherited config");
        assert_eq!(cfg.max_concurrent, 2);
        assert!(cfg.auto_approve_in_sandbox);
        assert_eq!(cfg.env_pass, vec!["ANTHROPIC_API_KEY"]);
        assert_eq!(cfg.sandbox, SandboxConfig::Inherited);
        assert_eq!(cfg.defaults["worker"].provider, Provider::Opencode);

        let valid_bwrap = r#"
max_concurrent = 4
auto_approve_in_sandbox = false

[sandbox]
kind = "bwrap"

[sandbox.bwrap]
home_ro = [".cargo", ".rustup"]
"#;
        let cfg = parse_loops_config(valid_bwrap).expect("valid bwrap config");
        assert_eq!(cfg.max_concurrent, 4);
        assert!(!cfg.auto_approve_in_sandbox);
        assert_eq!(
            cfg.sandbox,
            SandboxConfig::Bwrap {
                home_ro: vec![".cargo".into(), ".rustup".into()],
            }
        );

        let incus = r#"
[sandbox]
kind = "incus"

[sandbox.incus]
instance = "dev"
user = "ubuntu"
"#;
        let err = parse_loops_config(incus).unwrap_err();
        assert!(
            err.to_string().contains("ainda não suportado nesta versão"),
            "unexpected incus error: {err}"
        );

        let unknown_top = r#"
bogus_key = 1
[sandbox]
kind = "inherited"
"#;
        assert!(parse_loops_config(unknown_top).is_err());

        let unknown_sandbox = r#"
[sandbox]
kind = "inherited"
extra = true
"#;
        assert!(parse_loops_config(unknown_sandbox).is_err());

        let unknown_bwrap = r#"
[sandbox]
kind = "bwrap"
[sandbox.bwrap]
extra = []
"#;
        assert!(parse_loops_config(unknown_bwrap).is_err());
        for bad_path in ["/etc/passwd", "../secret", "a/../../b", "~/ssh", "", "."] {
            let cfg_rw = format!(
                r#"
[sandbox]
kind = "bwrap"
[sandbox.bwrap]
home_ro = ["{bad_path}"]
"#
            );
            assert!(
                parse_loops_config(&cfg_rw).is_err(),
                "expected home_ro='{bad_path}' to be rejected"
            );

            let cfg_ro = format!(
                r#"
[sandbox]
kind = "bwrap"
[sandbox.bwrap]
home_ro = ["{bad_path}"]
"#
            );
            assert!(
                parse_loops_config(&cfg_ro).is_err(),
                "expected home_ro='{bad_path}' to be rejected"
            );
        }
    }

    #[test]
    fn home_paths_never_expose_tchelvis_itself() {
        for bad in [
            ".config",
            ".config/tchelvis",
            ".config/tchelvis/loops.toml",
            "./.config/",
            ".local",
            ".local/state",
            ".local/share/tchelvis",
        ] {
            let raw =
                format!("[sandbox]\nkind = \"bwrap\"\n[sandbox.bwrap]\nhome_ro = [{bad:?}]\n");
            assert!(parse_loops_config(&raw).is_err(), "{bad}");
        }
        let ok = "[sandbox]\nkind = \"bwrap\"\n[sandbox.bwrap]\nhome_ro = [\".cargo\", \".rustup\", \".gitconfig\"]\n";
        assert!(parse_loops_config(ok).is_ok());
    }

    #[test]
    fn no_home_path_can_be_writable() {
        // Anything writable there (hooks, plugins, shell files) would later run outside.
        let raw = "[sandbox]\nkind = \"bwrap\"\n[sandbox.bwrap]\nhome_rw = [\".claude\"]\n";
        let err = format!("{:#}", parse_loops_config(raw).unwrap_err());
        assert!(err.contains("home_rw"), "{err}");
    }

    #[test]
    fn bwrap_is_the_default_sandbox() {
        for raw in [
            "",
            "max_concurrent = 1",
            "[sandbox]",
            "[sandbox]\nkind = \"bwrap\"",
        ] {
            let config = parse_loops_config(raw).unwrap();
            assert_eq!(
                config.sandbox,
                SandboxConfig::Bwrap { home_ro: vec![] },
                "{raw:?}"
            );
            // The default never turns automatic approval on.
            assert!(!config.auto_approve_in_sandbox, "{raw:?}");
        }
        let config = parse_loops_config("[sandbox.bwrap]\nhome_ro = [\".cargo\"]").unwrap();
        assert_eq!(
            config.sandbox,
            SandboxConfig::Bwrap {
                home_ro: vec![".cargo".into()]
            }
        );
        // A wrong value is an error, never the default.
        for raw in [
            "[sandbox]\nkind = \"\"",
            "[sandbox]\nkind = \"Bwrap\"",
            "[sandbox]\nknd = \"inherited\"",
        ] {
            assert!(parse_loops_config(raw).is_err(), "{raw:?}");
        }
        let inherited = parse_loops_config("[sandbox]\nkind = \"inherited\"").unwrap();
        assert_eq!(inherited.sandbox, SandboxConfig::Inherited);
    }

    #[test]
    fn defaults_model_without_provider_is_rejected() {
        let raw = r#"
[defaults.worker]
model = "claude-sonnet-4"
"#;
        let err = parse_loops_config(raw).unwrap_err();
        assert!(err.to_string().contains("provider"), "got: {err}");
    }

    #[test]
    fn preflight_config_parses_modes_and_valid_hours_bounds() {
        let def = parse_loops_config("").unwrap();
        assert_eq!(def.preflight.mode, PreflightMode::Auto);
        assert_eq!(def.preflight.valid_hours, 12);

        for (raw_mode, expected) in [
            ("auto", PreflightMode::Auto),
            ("always", PreflightMode::Always),
            ("free", PreflightMode::Free),
        ] {
            let cfg = parse_loops_config(&format!(
                "[preflight]\nmode = \"{raw_mode}\"\nvalid_hours = 24\n"
            ))
            .unwrap();
            assert_eq!(cfg.preflight.mode, expected);
            assert_eq!(cfg.preflight.valid_hours, 24);
        }

        assert_eq!(
            parse_loops_config("[preflight]\nvalid_hours = 1\n")
                .unwrap()
                .preflight
                .valid_hours,
            1
        );
        assert_eq!(
            parse_loops_config("[preflight]\nvalid_hours = 168\n")
                .unwrap()
                .preflight
                .valid_hours,
            168
        );

        assert!(parse_loops_config("[preflight]\nvalid_hours = 0\n").is_err());
        assert!(parse_loops_config("[preflight]\nvalid_hours = 169\n").is_err());
        assert!(parse_loops_config("[preflight]\nmode = \"bogus\"\n").is_err());
        assert!(parse_loops_config("[preflight]\nextra = true\n").is_err());
    }
}
