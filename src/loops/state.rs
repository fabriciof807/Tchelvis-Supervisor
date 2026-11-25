use crate::loops::models::ModelSource;
use crate::loops::preflight::ProbeRecord;
use crate::loops::verdict::{Verdict, MAX_SUMMARY_CHARS};
use crate::loops::workflow::Provider;
use crate::model::sanitize_terminal_text;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

pub const LOOP_STATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_RESULT_FILES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum RunStatus {
    Running,
    Pass,
    Error,
    Escalate,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Pass => "PASS",
            Self::Error => "ERROR",
            Self::Escalate => "ESCALATE",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Pass => 0,
            Self::Error | Self::Running => 1,
            Self::Escalate => 3,
            Self::Cancelled => 4,
        }
    }

    pub fn is_final(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub turns: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Total {
    pub value: u64,
    pub partial: bool,
}

pub fn format_thousands_pt(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().rev().enumerate() {
        if idx > 0 && idx % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

impl Total {
    pub fn format_pt(&self) -> String {
        let num = format_thousands_pt(self.value);
        if self.partial {
            format!("≥ {num}")
        } else {
            num
        }
    }
}

impl fmt::Display for Total {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_pt())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CostTotal {
    pub value: f64,
    pub partial: bool,
}

impl CostTotal {
    pub fn format_usd(&self) -> String {
        if self.partial {
            format!("≥ ${:.4}", self.value)
        } else {
            format!("${:.4}", self.value)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageTotals {
    pub turns: Total,
    pub input_tokens: Total,
    pub output_tokens: Total,
    pub cache_read_tokens: Total,
    pub cost_usd: Option<CostTotal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerInfo {
    pub terminal: String,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Execution {
    pub cycle: u32,
    pub stage_id: String,
    pub role: String,
    pub provider: Provider,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<ModelSource>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub duration_ms: u64,
    pub usage: Usage,
    pub verdict: Option<Verdict>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub schema_version: u32,
    pub run_id: String,
    pub workflow: String,
    pub status: RunStatus,
    #[serde(default)]
    pub reason: Option<String>,
    pub sandbox: String,
    #[serde(default)]
    pub sandbox_exposures: Vec<String>,
    #[serde(default)]
    pub preflight: Vec<ProbeRecord>,
    pub current_cycle: u32,
    pub max_cycles: u32,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub engine_pid: Option<u32>,
    #[serde(default)]
    pub engine_start_ticks: Option<u64>,
    #[serde(default)]
    pub caller: Option<CallerInfo>,
    pub heartbeat_at_ms: i64,
    pub last_activity_at_ms: i64,
    pub started_at_ms: i64,
    #[serde(default)]
    pub finished_at_ms: Option<i64>,
    #[serde(default)]
    pub executions: Vec<Execution>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub summary: String,
}

pub fn redact_loop_text(raw: &str, extra_secrets: &[&str]) -> String {
    let mut out = crate::logging::redact_secrets(raw);
    for secret in extra_secrets {
        if !secret.is_empty() {
            out = out.replace(secret, "[REDACTED]");
        }
    }
    out
}

pub fn sanitize_single_line(raw: &str, max_chars: Option<usize>) -> String {
    let single = raw.replace(['\r', '\n'], " ");
    let cleaned = sanitize_terminal_text(&single);
    match max_chars {
        Some(limit) => cleaned
            .chars()
            .take(limit)
            .collect::<String>()
            .trim()
            .to_string(),
        None => cleaned,
    }
}

fn sum_u64_field(executions: &[Execution], extract: impl Fn(&Usage) -> Option<u64>) -> Total {
    let mut total = Total::default();
    for exec in executions {
        match extract(&exec.usage) {
            Some(v) => total.value = total.value.saturating_add(v),
            None => total.partial = true,
        }
    }
    total
}

pub fn sum_usage(executions: &[Execution]) -> UsageTotals {
    let turns = sum_u64_field(executions, |u| u.turns);
    let input_tokens = sum_u64_field(executions, |u| u.input_tokens);
    let output_tokens = sum_u64_field(executions, |u| u.output_tokens);
    let cache_read_tokens = sum_u64_field(executions, |u| u.cache_read_tokens);

    let mut cost_sum = 0.0_f64;
    let mut cost_any = false;
    let mut cost_partial = false;
    for exec in executions {
        match exec.usage.cost_usd {
            Some(c) if c.is_finite() => {
                cost_any = true;
                cost_sum += c;
            }
            _ => {
                cost_partial = true;
            }
        }
    }
    let cost_usd = if cost_any {
        Some(CostTotal {
            value: cost_sum,
            partial: cost_partial,
        })
    } else {
        None
    };

    UsageTotals {
        turns,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cost_usd,
    }
}

pub fn save_state(dir: &Path, s: &RunState) -> Result<()> {
    let tmp_path = dir.join("state.json.tmp");
    let final_path = dir.join("state.json");
    let json = serde_json::to_vec_pretty(s).context("falha ao serializar state.json")?;

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("falha ao abrir {}", tmp_path.display()))?;
        file.write_all(&json)?;
        file.sync_all()?;
    }

    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "falha ao renomear {} para {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;

    if let Ok(dir_handle) = fs::File::open(dir) {
        let _ = dir_handle.sync_all();
    }

    Ok(())
}

pub fn parse_state(raw: &str) -> Result<RunState> {
    let val: serde_json::Value =
        serde_json::from_str(raw).context("JSON inválido em state.json")?;
    let Some(version) = val.get("schema_version").and_then(|v| v.as_u64()) else {
        bail!("campo obrigatório 'schema_version' ausente ou inválido em state.json");
    };
    if version != u64::from(LOOP_STATE_SCHEMA_VERSION) {
        bail!(
            "schema_version desconhecida em state.json: {version} (suportada: {LOOP_STATE_SCHEMA_VERSION})"
        );
    }
    serde_json::from_value(val).context("falha ao desserializar RunState")
}

pub fn load_state(dir: &Path) -> Result<RunState> {
    let path = dir.join("state.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("falha ao ler {}", path.display()))?;
    parse_state(&raw)
}

/// Line 1 of `loop result` and of `loop status`. The unsandboxed mode must never be silent.
pub fn result_line_one(
    status: &RunStatus,
    workflow: &str,
    cycle: u32,
    max: u32,
    sandbox: &str,
) -> String {
    let line = format!("STATUS: {status} | LOOP: {workflow} | CICLOS: {cycle}/{max}");
    if sandbox == "none" {
        format!("{line} | SEM SANDBOX")
    } else {
        line
    }
}

pub fn format_result(s: &RunState) -> String {
    let workflow = sanitize_single_line(&s.workflow, None);
    let run_id = sanitize_single_line(&s.run_id, None);
    let branch = s
        .branch
        .as_deref()
        .map(|b| sanitize_single_line(b, None))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "(nenhuma)".to_string());

    let files_line = if s.files.is_empty() {
        "(nenhum)".to_string()
    } else {
        let shown: Vec<String> = s
            .files
            .iter()
            .take(MAX_RESULT_FILES)
            .map(|f| sanitize_single_line(f, None))
            .collect();
        if s.files.len() > MAX_RESULT_FILES {
            format!(
                "{} (+{})",
                shown.join(", "),
                s.files.len() - MAX_RESULT_FILES
            )
        } else {
            shown.join(", ")
        }
    };

    let raw_summary = if s.summary.trim().is_empty() {
        s.reason.as_deref().unwrap_or("")
    } else {
        &s.summary
    };
    let summary = sanitize_single_line(raw_summary, Some(MAX_SUMMARY_CHARS));

    format!(
        "{}\nRUN_ID: {}\nBRANCH: {}\nARQUIVOS: {}\nRESUMO: {}",
        result_line_one(
            &s.status,
            &workflow,
            s.current_cycle,
            s.max_cycles,
            &s.sandbox
        ),
        run_id,
        branch,
        files_line,
        summary
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(status: RunStatus) -> RunState {
        RunState {
            schema_version: LOOP_STATE_SCHEMA_VERSION,
            run_id: "20261123-182500-a1b2".to_string(),
            workflow: "worker-reviewer-critic".to_string(),
            status,
            reason: Some("motivo de teste".to_string()),
            sandbox: "bwrap".to_string(),
            sandbox_exposures: vec![],
            preflight: vec![],
            current_cycle: 2,
            max_cycles: 3,
            branch: Some("loop/worker-reviewer-critic-20261123-182500-a1b2".to_string()),
            base_commit: Some("deadbeef".to_string()),
            engine_pid: Some(1234),
            engine_start_ticks: Some(5678),
            caller: Some(CallerInfo {
                terminal: "t1".to_string(),
                created_ms: 1000,
            }),
            heartbeat_at_ms: 2000,
            last_activity_at_ms: 2000,
            started_at_ms: 1000,
            finished_at_ms: Some(3000),
            executions: vec![],
            files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            summary: "Tudo implementado e verificado.".to_string(),
        }
    }

    #[test]
    fn format_result_is_five_lines_for_every_status() {
        let expected_codes = [
            (RunStatus::Running, 1, false),
            (RunStatus::Pass, 0, true),
            (RunStatus::Error, 1, true),
            (RunStatus::Escalate, 3, true),
            (RunStatus::Cancelled, 4, true),
        ];

        for (status, code, is_final) in expected_codes {
            assert_eq!(status.exit_code(), code);
            assert_eq!(status.is_final(), is_final);

            let mut s = sample_state(status);
            s.files = (0..500)
                .map(|i| format!("src/f{i}\n\x1b[31m.rs\x1b[0m"))
                .collect();
            s.summary = format!(
                "\x1b[33mLinha 1\nLinha 2\x1b]7501;state=done\x1b\\\x1b[0m {}",
                "a".repeat(1024 * 1024)
            );

            let out = format_result(&s);
            let lines: Vec<&str> = out.split('\n').collect();
            assert_eq!(
                lines.len(),
                5,
                "expected 5 lines for {status:?}, got:\n{out}"
            );
            assert_eq!(out.lines().count(), 5);
            assert!(lines[0].starts_with(&format!("STATUS: {status} | LOOP: ")));
            assert!(lines[1].starts_with("RUN_ID: "));
            assert!(lines[2].starts_with("BRANCH: "));
            assert!(lines[3].starts_with("ARQUIVOS: "));
            assert!(lines[3].ends_with(" (+490)"), "line 4 was: {}", lines[3]);
            assert!(!lines[3].contains('\x1b'));
            assert!(lines[4].starts_with("RESUMO: "));
            let resumo_body = lines[4].strip_prefix("RESUMO: ").unwrap();
            assert!(
                resumo_body.chars().count() <= 140,
                "resumo too long: {}",
                resumo_body.chars().count()
            );
            assert!(!resumo_body.contains('\x1b'));
        }
    }

    #[test]
    fn totals_with_null_are_partial() {
        let execs = vec![
            Execution {
                cycle: 1,
                stage_id: "implement".into(),
                role: "worker".into(),
                provider: Provider::Opencode,
                model: None,
                model_source: None,
                started_at_ms: 1,
                finished_at_ms: Some(2),
                duration_ms: 1,
                usage: Usage {
                    turns: Some(3),
                    input_tokens: Some(18_420),
                    output_tokens: Some(500),
                    cache_read_tokens: None,
                    cost_usd: Some(0.12),
                },
                verdict: None,
                outcome: "OK".into(),
                error: None,
                log_file: None,
            },
            Execution {
                cycle: 1,
                stage_id: "review".into(),
                role: "reviewer".into(),
                provider: Provider::Agy,
                model: None,
                model_source: None,
                started_at_ms: 2,
                finished_at_ms: Some(3),
                duration_ms: 1,
                usage: Usage {
                    turns: Some(2),
                    input_tokens: None,
                    output_tokens: Some(100),
                    cache_read_tokens: None,
                    cost_usd: None,
                },
                verdict: Some(Verdict::Pass),
                outcome: "OK".into(),
                error: None,
                log_file: None,
            },
        ];

        let totals = sum_usage(&execs);
        assert_eq!(
            totals.input_tokens,
            Total {
                value: 18_420,
                partial: true
            }
        );
        assert_eq!(totals.input_tokens.format_pt(), "≥ 18.420");
        assert_eq!(
            totals.output_tokens,
            Total {
                value: 600,
                partial: false
            }
        );
        assert_eq!(totals.output_tokens.format_pt(), "600");
        assert_eq!(
            totals.turns,
            Total {
                value: 5,
                partial: false
            }
        );
    }

    #[test]
    fn state_round_trips_and_survives_a_leftover_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json.tmp"), "corrupt leftover").unwrap();

        let state = sample_state(RunStatus::Pass);
        save_state(dir.path(), &state).unwrap();
        assert!(!dir.path().join("state.json.tmp").exists());

        let loaded = load_state(dir.path()).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn unknown_schema_version_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), r#"{"schema_version": 99}"#).unwrap();
        let err = load_state(dir.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "got: {err}");

        std::fs::write(dir.path().join("state.json"), r#"{"schema_version": 0}"#).unwrap();
        let err = load_state(dir.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "got: {err}");
    }

    #[test]
    fn old_state_json_without_model_source_or_preflight_loads_and_new_fields_round_trip() {
        let legacy_json = r#"{
  "schema_version": 1,
  "run_id": "20261123-182500-a1b2",
  "workflow": "worker-reviewer-critic",
  "status": "PASS",
  "sandbox": "bwrap",
  "current_cycle": 1,
  "max_cycles": 3,
  "heartbeat_at_ms": 2000,
  "last_activity_at_ms": 2000,
  "started_at_ms": 1000,
  "executions": [
    {
      "cycle": 1,
      "stage_id": "implement",
      "role": "worker",
      "provider": "claude",
      "model": null,
      "started_at_ms": 1000,
      "finished_at_ms": 1500,
      "duration_ms": 500,
      "usage": {
        "turns": 1,
        "input_tokens": 10,
        "output_tokens": 5,
        "cache_read_tokens": 0,
        "cost_usd": 0.01
      },
      "verdict": null,
      "outcome": "OK"
    }
  ]
}"#;
        let legacy =
            parse_state(legacy_json).expect("legacy JSON without model_source/preflight must load");
        assert!(legacy.preflight.is_empty());
        assert_eq!(legacy.executions[0].model_source, None);

        let mut modern = sample_state(RunStatus::Pass);
        modern.preflight.push(ProbeRecord {
            provider: Provider::Claude,
            model: Some("haiku".to_string()),
            method: crate::loops::preflight::ProbeMethod::Probe,
            usage: Usage {
                turns: Some(1),
                input_tokens: Some(3500),
                output_tokens: Some(10),
                cache_read_tokens: Some(0),
                cost_usd: Some(0.0006),
            },
            duration_ms: 2400,
        });
        modern.executions.push(Execution {
            cycle: 1,
            stage_id: "implement".into(),
            role: "worker".into(),
            provider: Provider::Claude,
            model: Some("haiku".into()),
            model_source: Some(ModelSource::ProjectStage),
            started_at_ms: 1000,
            finished_at_ms: Some(2000),
            duration_ms: 1000,
            usage: Usage::default(),
            verdict: None,
            outcome: "OK".into(),
            error: None,
            log_file: None,
        });
        let dir = tempfile::tempdir().unwrap();
        save_state(dir.path(), &modern).unwrap();
        let reloaded = load_state(dir.path()).unwrap();
        assert_eq!(reloaded, modern);
    }

    #[test]
    fn format_result_flags_unsandboxed_runs() {
        for status in [
            RunStatus::Running,
            RunStatus::Pass,
            RunStatus::Error,
            RunStatus::Escalate,
            RunStatus::Cancelled,
        ] {
            let mut s = sample_state(status);
            s.sandbox = "none".to_string();
            let out = format_result(&s);
            assert!(
                out.lines().next().unwrap().ends_with(" | SEM SANDBOX"),
                "{out}"
            );
            assert_eq!(out.lines().count(), 5, "{out}");
        }

        let out = format_result(&sample_state(RunStatus::Pass));
        assert!(!out.contains("SEM SANDBOX"), "{out}");
    }
}
