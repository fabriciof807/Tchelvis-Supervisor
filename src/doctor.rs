use crate::assets::{
    asset_sha256, CLAUDE_HOOK_SH, HERDR_LAYOUT_JSON, HERDR_MOD_JSON, HERDR_MOD_MAIN_JS, SHELL_SH,
};
use crate::config::{CockpitConfig, CockpitPaths};
use crate::herdr::HerdrClient;
use crate::logging::redact_secrets;
use crate::model::{ProviderHealth, ProviderId};
use crate::providers::ProviderCoordinator;
use crate::store::CacheStore;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub overall_status: String,
    pub cockpit_version: String,
    pub checks: Vec<DoctorCheck>,
    pub providers: BTreeMap<ProviderId, ProviderHealth>,
}

pub async fn run_doctor(
    cfg: &CockpitConfig,
    paths: &CockpitPaths,
    bundle_out: Option<&Path>,
) -> Result<DoctorReport> {
    let mut checks = Vec::new();
    let mut has_fail = false;

    // 1. Check herdr-core (hc) runtime & RPC connectivity
    let herdr = HerdrClient::default();
    match herdr.probe_version().await {
        Ok(ver) => {
            let mut rpc = None;
            let rpc_ok = herdr.list_terminals_full(&mut rpc).await.is_ok();
            checks.push(DoctorCheck {
                name: "hc_runtime".into(),
                status: if rpc_ok { "ok".into() } else { "warn".into() },
                detail: format!("{ver} (rpc_connected={rpc_ok})"),
            });
        }
        Err(e) => {
            has_fail = true;
            checks.push(DoctorCheck {
                name: "hc_runtime".into(),
                status: "fail".into(),
                detail: format!("hc not reachable: {e}"),
            });
        }
    }

    // 2. Check hooks integrity
    let hooks_dir = paths.hooks_dir();
    let claude_hook = hooks_dir.join("claude-hook.sh");
    let shell_hook = hooks_dir.join("shell.sh");
    if claude_hook.exists() && shell_hook.exists() {
        let c_ok = fs::read_to_string(&claude_hook)
            .map(|s| asset_sha256(&s) == asset_sha256(CLAUDE_HOOK_SH))
            .unwrap_or(false);
        let s_ok = fs::read_to_string(&shell_hook)
            .map(|s| asset_sha256(&s) == asset_sha256(SHELL_SH))
            .unwrap_or(false);
        checks.push(DoctorCheck {
            name: "hooks".into(),
            status: if c_ok && s_ok {
                "ok".into()
            } else {
                "warn".into()
            },
            detail: format!("claude_hook_match={c_ok}, shell_hook_match={s_ok}"),
        });
    } else {
        checks.push(DoctorCheck {
            name: "hooks".into(),
            status: "not_installed".into(),
            detail: "Run `cockpit setup` to install hook scripts".into(),
        });
    }

    // 3. Check Herdr mod & layout schema/hash alignment
    let mod_path = paths
        .home_dir
        .join(".config/herdr-app/mods/cockpit/main.js");
    let mod_json_path = paths
        .home_dir
        .join(".config/herdr-app/mods/cockpit/mod.json");
    let layout_path = paths
        .home_dir
        .join(".config/herdr-app/layouts/cockpit.json");
    let embedded_layout_valid = serde_json::from_str::<serde_json::Value>(HERDR_LAYOUT_JSON)
        .map(|v| {
            v.get("id").and_then(|n| n.as_str()) == Some("root")
                && matches!(v.get("type").and_then(|t| t.as_str()), Some("row" | "column"))
        })
        .unwrap_or(false);

    if mod_path.exists() && mod_json_path.exists() && layout_path.exists() {
        let m_ok = fs::read_to_string(&mod_path)
            .map(|s| asset_sha256(&s) == asset_sha256(HERDR_MOD_MAIN_JS))
            .unwrap_or(false);
        let mj_ok = fs::read_to_string(&mod_json_path)
            .map(|s| asset_sha256(&s) == asset_sha256(HERDR_MOD_JSON))
            .unwrap_or(false);
        let l_ok = fs::read_to_string(&layout_path)
            .map(|s| asset_sha256(&s) == asset_sha256(HERDR_LAYOUT_JSON))
            .unwrap_or(false);
        checks.push(DoctorCheck {
            name: "herdr_mod".into(),
            status: if m_ok && mj_ok && l_ok && embedded_layout_valid {
                "ok".into()
            } else {
                "warn".into()
            },
            detail: format!(
                "mod_hash_match={m_ok}, mod_json_hash_match={mj_ok}, layout_hash_match={l_ok}, schema_valid={embedded_layout_valid}"
            ),
        });
    } else {
        checks.push(DoctorCheck {
            name: "herdr_mod".into(),
            status: "not_installed".into(),
            detail: format!(
                "Embedded layout schema valid={embedded_layout_valid}; run `cockpit setup` to install to ~/.config/herdr-app/"
            ),
        });
    }

    // 4. Check SQLite store & runtime directory
    let _ = paths.ensure_dirs();
    let store = match CacheStore::open(&paths.db_file()) {
        Ok(s) => {
            let sock_exists = paths.socket_file().exists();
            checks.push(DoctorCheck {
                name: "storage_ipc".into(),
                status: "ok".into(),
                detail: format!(
                    "SQLite WAL ready at {}; socket_present={sock_exists}",
                    paths.db_file().display()
                ),
            });
            s
        }
        Err(e) => {
            has_fail = true;
            checks.push(DoctorCheck {
                name: "storage_ipc".into(),
                status: "fail".into(),
                detail: format!("SQLite error: {e}"),
            });
            CacheStore::open_in_memory()?
        }
    };

    // 5. Check all 6 providers
    let coord = ProviderCoordinator::new();
    let providers = coord.check_all_health(cfg, &store, &paths.home_dir).await;

    // 6. Check Durable Supervisor configuration & state
    let sup_cfg = crate::supervisor::config::SupervisorConfig::load();
    let legacy_state = crate::supervisor::config::SupervisorConfig::legacy_state_file_path();
    let sup_mode = crate::supervisor::state::DurableStateStore::load_or_init_with_fallback(
        &sup_cfg.state_file_path,
        &legacy_state,
    )
    .map(|s| s.mode.label().to_string())
    .unwrap_or_else(|_| "unknown".to_string());
    let gpu_mode = if sup_cfg.effective_num_gpu() == 0 {
        "num_gpu=0 (CPU-only)".to_string()
    } else {
        format!("num_gpu={}", sup_cfg.effective_num_gpu())
    };
    checks.push(DoctorCheck {
        name: "supervisor".into(),
        status: "ok".into(),
        detail: format!(
            "ollama_host={}, model_name={}, {}, mode={}",
            sup_cfg.ollama_host, sup_cfg.ollama_model, gpu_mode, sup_mode
        ),
    });

    let report = DoctorReport {
        overall_status: if has_fail {
            "degraded".into()
        } else {
            "ok".into()
        },
        cockpit_version: env!("CARGO_PKG_VERSION").to_string(),
        checks,
        providers,
    };

    if let Some(out_path) = bundle_out {
        let raw_json = serde_json::to_string_pretty(&report)?;
        let redacted = redact_secrets(&raw_json);
        if let Some(parent) = out_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(out_path, redacted + "\n")?;
    }

    Ok(report)
}
