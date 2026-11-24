use crate::loops::workflow::RoleDefaults;
use anyhow::{bail, Context, Result};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopsConfig {
    pub max_concurrent: u32,
    pub auto_approve_in_sandbox: bool,
    pub env_pass: Vec<String>,
    pub sandbox: SandboxConfig,
    pub defaults: RoleDefaults,
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
    sandbox: RawSandbox,
    #[serde(default)]
    defaults: RoleDefaults,
}

fn default_max_concurrent() -> u32 {
    DEFAULT_MAX_CONCURRENT
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSandbox {
    kind: String,
    #[serde(default)]
    bwrap: Option<RawBwrap>,
    #[serde(default)]
    incus: Option<RawIncus>,
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
    let parsed: RawLoopsConfig = toml::from_str(raw).context("configuração loops.toml inválida")?;

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
            if model.trim().is_empty() {
                bail!("'model' em [defaults.{role}] não pode ser vazio");
            }
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

    Ok(LoopsConfig {
        max_concurrent: parsed.max_concurrent,
        auto_approve_in_sandbox: parsed.auto_approve_in_sandbox,
        env_pass: parsed.env_pass,
        sandbox,
        defaults: parsed.defaults,
    })
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

        let missing_sandbox = r#"
max_concurrent = 2
"#;
        assert!(parse_loops_config(missing_sandbox).is_err());

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
}
