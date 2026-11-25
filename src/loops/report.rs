use crate::loops::artifacts::MAX_ARTIFACT_BYTES;
use crate::loops::state::{redact_loop_text, sanitize_single_line, sum_usage, RunState};
use crate::loops::verdict::MAX_SUMMARY_CHARS;
use regex::Regex;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::sync::LazyLock;

static ANSI_CSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("valid CSI regex"));
static ANSI_OSC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").expect("valid OSC regex"));

fn sanitize_line(raw: &str, max_chars: Option<usize>, extra_secrets: &[&str]) -> String {
    let single = sanitize_single_line(raw, max_chars);
    let redacted = redact_loop_text(&single, extra_secrets);
    match max_chars {
        Some(limit) => redacted
            .chars()
            .take(limit)
            .collect::<String>()
            .trim()
            .to_string(),
        None => redacted,
    }
}

pub(crate) fn sanitize_multiline(raw: &str, extra_secrets: &[&str]) -> String {
    let without_osc = ANSI_OSC_RE.replace_all(raw, "");
    let without_csi = ANSI_CSI_RE.replace_all(&without_osc, "");
    let mut clean = String::with_capacity(without_csi.len());
    for ch in without_csi.chars() {
        let code = ch as u32;
        if ch == '\n' {
            clean.push('\n');
        } else if ch == '\t' {
            clean.push(' ');
        } else if code <= 0x1F || (0x7F..=0x9F).contains(&code) {
            continue;
        } else {
            clean.push(ch);
        }
    }
    redact_loop_text(&clean, extra_secrets).trim().to_string()
}

fn escape_table_cell(raw: &str) -> String {
    raw.replace('|', "\\|")
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        let mins = secs / 60;
        let rem_secs = secs % 60;
        format!("{mins}m {rem_secs}s")
    }
}

fn read_artifact_safe(dir: &Path, name: &str) -> Option<String> {
    let path = dir.join(name);
    let raw_path = path.to_str()?;
    let c_path = CString::new(raw_path).ok()?;
    // SAFETY: c_path is NUL-terminated; O_NOFOLLOW refuses symlinks and O_NONBLOCK avoids blocking on FIFOs.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is a newly opened descriptor owned by us.
    let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: owned_fd is a valid open file descriptor.
    if unsafe { libc::fstat(owned_fd.as_raw_fd(), &mut st) } < 0 {
        return None;
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return None;
    }
    if st.st_size < 0 || st.st_size as usize > MAX_ARTIFACT_BYTES {
        return None;
    }
    let file = File::from(owned_fd);
    let mut buf = Vec::with_capacity(st.st_size as usize);
    file.take((MAX_ARTIFACT_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > MAX_ARTIFACT_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Every text artifact of the run: `input.md` first, then the current ones, then each cycle's copy,
/// which tells why an earlier cycle was rejected.
fn artifact_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".md") && n != "report.md")
                .collect()
        })
        .unwrap_or_default();
    let cycle = |n: &str| -> u32 {
        n.strip_prefix('c')
            .and_then(|r| r.split_once('-'))
            .and_then(|(digits, _)| digits.parse().ok())
            .unwrap_or(0)
    };
    names.sort_by_key(|n| (n != "input.md", cycle(n), n.clone()));
    // The last copy of each artifact equals the current file.
    let text = |n: &str| std::fs::read(dir.join(n)).ok();
    let current: Vec<String> = names.iter().filter(|n| cycle(n) == 0).cloned().collect();
    names.retain(|n| {
        cycle(n) == 0
            || !current
                .iter()
                .any(|c| n.ends_with(&format!("-{c}")) && text(n) == text(c))
    });
    names
}

fn fence_for(content: &str) -> String {
    let mut max_run = 2usize;
    let mut cur = 0usize;
    for ch in content.chars() {
        if ch == '`' {
            cur += 1;
            if cur > max_run {
                max_run = cur;
            }
        } else {
            cur = 0;
        }
    }
    "`".repeat(max_run + 1)
}

pub fn render_report(state: &RunState, dir: &Path) -> String {
    render_report_with_secrets(state, dir, &[])
}

pub fn render_report_with_secrets(state: &RunState, dir: &Path, extra_secrets: &[&str]) -> String {
    let mut out = String::new();
    let workflow = sanitize_line(&state.workflow, None, extra_secrets);
    let run_id = sanitize_line(&state.run_id, None, extra_secrets);
    let sandbox = sanitize_line(&state.sandbox, None, extra_secrets);
    let branch = state
        .branch
        .as_deref()
        .map(|b| sanitize_line(b, None, extra_secrets))
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "(nenhuma)".to_string());

    let total_duration_ms = match state.finished_at_ms {
        Some(fin) if fin >= state.started_at_ms => (fin - state.started_at_ms) as u64,
        _ => state.executions.iter().map(|e| e.duration_ms).sum(),
    };
    let totals = sum_usage(&state.executions);

    let _ = writeln!(out, "# Relatório do Loop — {workflow}\n");
    let _ = writeln!(out, "## Resumo\n");
    let _ = writeln!(out, "- **Status:** {}", state.status);
    let _ = writeln!(out, "- **Run ID:** `{run_id}`");
    let _ = writeln!(out, "- **Workflow:** `{workflow}`");
    let _ = writeln!(
        out,
        "- **Ciclos:** {}/{}",
        state.current_cycle, state.max_cycles
    );
    let _ = writeln!(out, "- **Branch:** `{branch}`");
    if let Some(base) = &state.base_commit {
        let _ = writeln!(
            out,
            "- **Commit base:** `{}`",
            sanitize_line(base, None, extra_secrets)
        );
    }
    let _ = writeln!(
        out,
        "- **Duração total:** {}",
        format_duration_ms(total_duration_ms)
    );
    if sandbox == "none" {
        let _ = writeln!(
            out,
            "- **Sandbox:** **SEM SANDBOX** (`none`): o agente rodou com o HOME, a rede e os poderes do seu usuário"
        );
    } else {
        let _ = writeln!(out, "- **Sandbox:** `{sandbox}`");
    }
    if state.sandbox_exposures.is_empty() {
        let _ = writeln!(out, "- **Exposições do sandbox:** nenhuma detectada");
    } else {
        let exposures: Vec<String> = state
            .sandbox_exposures
            .iter()
            .map(|e| sanitize_line(e, None, extra_secrets))
            .collect();
        let _ = writeln!(out, "- **Exposições do sandbox:** {}", exposures.join("; "));
    }
    if let Some(reason) = &state.reason {
        let clean_reason = sanitize_line(reason, None, extra_secrets);
        if !clean_reason.is_empty() {
            let _ = writeln!(out, "- **Motivo:** {clean_reason}");
        }
    }
    let clean_summary = sanitize_line(&state.summary, Some(MAX_SUMMARY_CHARS), extra_secrets);
    if !clean_summary.is_empty() {
        let _ = writeln!(out, "- **Resumo final:** {clean_summary}");
    }

    let _ = writeln!(out, "\n### Consumo total\n");
    let _ = writeln!(out, "- **Turnos:** {}", totals.turns.format_pt());
    let _ = writeln!(
        out,
        "- **Tokens de entrada:** {}",
        totals.input_tokens.format_pt()
    );
    let _ = writeln!(
        out,
        "- **Tokens de saída:** {}",
        totals.output_tokens.format_pt()
    );
    let _ = writeln!(
        out,
        "- **Tokens de cache (leitura):** {}",
        totals.cache_read_tokens.format_pt()
    );
    let cost_str = totals
        .cost_usd
        .map(|c| c.format_usd())
        .unwrap_or_else(|| "N/A".to_string());
    let _ = writeln!(out, "- **Custo estimado:** {cost_str}");

    if !state.preflight.is_empty() {
        let _ = writeln!(
            out,
            "\n### Pré-teste de modelos (fora dos totais do loop)\n"
        );
        let _ = writeln!(
            out,
            "| Provider | Modelo | Método | Turnos | Tokens (In / Out / Cache) | Custo | Duração |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|");
        for probe in &state.preflight {
            let model = probe
                .model
                .as_deref()
                .map(|m| escape_table_cell(&sanitize_line(m, None, extra_secrets)))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "-".to_string());
            let method = probe.method.as_str();
            let turns = probe
                .usage
                .turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let tok_in = probe
                .usage
                .input_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let tok_out = probe
                .usage
                .output_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let tok_cache = probe
                .usage
                .cache_read_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let cost = probe
                .usage
                .cost_usd
                .filter(|c| c.is_finite())
                .map(|c| format!("${c:.4}"))
                .unwrap_or_else(|| "N/A".to_string());
            let dur = format_duration_ms(probe.duration_ms);
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} / {} / {} | {} | {} |",
                probe.provider, model, method, turns, tok_in, tok_out, tok_cache, cost, dur
            );
        }
    }

    let _ = writeln!(out, "\n## Arquivos alterados\n");
    if state.files.is_empty() {
        let _ = writeln!(out, "(nenhum)");
    } else {
        for f in &state.files {
            let clean = sanitize_line(f, None, extra_secrets);
            if !clean.is_empty() {
                let _ = writeln!(out, "- `{clean}`");
            }
        }
    }

    let _ = writeln!(out, "\n## Linha do tempo\n");
    if state.executions.is_empty() {
        let _ = writeln!(out, "(nenhuma etapa executada)");
    } else {
        let _ = writeln!(
            out,
            "| Ciclo | Estágio | Role | Provider | Modelo | Origem | Turnos | Tokens (In / Out / Cache) | Custo | Duração | Resultado |"
        );
        let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|---|---|");
        for exec in &state.executions {
            let stage_id = escape_table_cell(&sanitize_line(&exec.stage_id, None, extra_secrets));
            let role = escape_table_cell(&sanitize_line(&exec.role, None, extra_secrets));
            let model = exec
                .model
                .as_deref()
                .map(|m| escape_table_cell(&sanitize_line(m, None, extra_secrets)))
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "-".to_string());
            let source = exec
                .model_source
                .map(|s| escape_table_cell(&sanitize_line(s.as_str(), None, extra_secrets)))
                .unwrap_or_else(|| "-".to_string());
            let turns = exec
                .usage
                .turns
                .map(|t| t.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let tok_in = exec
                .usage
                .input_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let tok_out = exec
                .usage
                .output_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let tok_cache = exec
                .usage
                .cache_read_tokens
                .map(|v| v.to_string())
                .unwrap_or_else(|| "N/A".to_string());
            let cost = exec
                .usage
                .cost_usd
                .filter(|c| c.is_finite())
                .map(|c| format!("${c:.4}"))
                .unwrap_or_else(|| "N/A".to_string());
            let dur = format_duration_ms(exec.duration_ms);
            let mut outcome = escape_table_cell(&sanitize_line(&exec.outcome, None, extra_secrets));
            if let Some(v) = exec.verdict {
                if !outcome.contains(v.as_str()) {
                    outcome = format!("{outcome} ({v})");
                }
            }
            if let Some(err) = &exec.error {
                let clean_err = escape_table_cell(&sanitize_line(err, Some(160), extra_secrets));
                if !clean_err.is_empty() {
                    outcome = format!("{outcome}: {clean_err}");
                }
            }
            let _ = writeln!(
                out,
                "| {} | `{}` | {} | {} | {} | {} | {} | {} / {} / {} | {} | {} | {} |",
                exec.cycle,
                stage_id,
                role,
                exec.provider,
                model,
                source,
                turns,
                tok_in,
                tok_out,
                tok_cache,
                cost,
                dur,
                outcome
            );
        }
    }

    let mut rendered_any = false;
    for filename in artifact_names(dir) {
        let heading = format!("`{filename}`");
        if let Some(raw) = read_artifact_safe(dir, &filename) {
            let clean = sanitize_multiline(&raw, extra_secrets);
            if clean.is_empty() {
                continue;
            }
            if !rendered_any {
                let _ = writeln!(out, "\n## Artefatos e apontamentos\n");
                rendered_any = true;
            }
            let fence = fence_for(&clean);
            let _ = writeln!(out, "### {heading}\n");
            let _ = writeln!(out, "{fence}markdown");
            let _ = writeln!(out, "{clean}");
            let _ = writeln!(out, "{fence}\n");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loops::state::{CallerInfo, Execution, RunStatus, Usage, LOOP_STATE_SCHEMA_VERSION};
    use crate::loops::verdict::Verdict;
    use crate::loops::workflow::Provider;
    use tempfile::TempDir;

    #[test]
    fn report_contains_status_timeline_partial_totals_sandbox_and_redacted_text() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        std::fs::write(
            dir.join("review.md"),
            "Achado com segredo Authorization: Bearer super_secret_token_999\n\x1b[31mANSI\x1b[0m e token ghp_1234567890abcdef1234\n",
        )
        .unwrap();
        std::fs::write(dir.join("critic.md"), "Tudo corrigido.\nVERDICT: PASS\n").unwrap();

        let state = rich_state();

        let md = render_report(&state, dir);

        assert!(md.contains("PASS"));
        assert!(md.contains("worker-reviewer-critic"));
        assert!(md.contains("2/3"));
        assert!(md.contains("bwrap"));
        assert!(md.contains("rede compartilhada com o host/container"));
        assert!(md.contains("serviços em localhost e sockets abstratos compartilhados"));
        // Timeline rows
        assert!(md.contains("implement"));
        assert!(md.contains("audit"));
        assert!(md.contains("RETRY"));
        assert!(md.contains("projeto (.loops/models.toml etapa)"));
        // Partial totals
        assert!(md.contains("≥ 18.920"));
        assert!(md.contains("≥ 1.200"));
        // Redacted and sanitized
        assert!(!md.contains("super_secret_token_999"));
        assert!(!md.contains("ghp_1234567890abcdef1234"));
        assert!(!md.contains("glpat-abcdefghijklmn"));
        assert!(!md.contains('\x1b'));
        assert!(md.contains("[REDACTED]"));
        assert!(md.contains("src/a.rs"));
    }

    #[test]
    fn report_renders_preflight_and_model_source_apart_from_loop_totals() {
        let tmp = TempDir::new().unwrap();
        let state = RunState {
            schema_version: LOOP_STATE_SCHEMA_VERSION,
            run_id: "20261123-182500-a1b2".to_string(),
            workflow: "fast-worker-reviewer".to_string(),
            status: RunStatus::Pass,
            reason: None,
            sandbox: "bwrap".to_string(),
            sandbox_exposures: vec![],
            preflight: vec![
                crate::loops::preflight::ProbeRecord {
                    provider: Provider::Claude,
                    model: Some("haiku".to_string()),
                    method: crate::loops::preflight::ProbeMethod::Probe,
                    usage: Usage {
                        turns: Some(1),
                        input_tokens: Some(9_000),
                        output_tokens: Some(50),
                        cache_read_tokens: Some(0),
                        cost_usd: Some(0.5000),
                    },
                    duration_ms: 2500,
                },
                crate::loops::preflight::ProbeRecord {
                    provider: Provider::Agy,
                    model: Some("gemini-4-hydrogen".to_string()),
                    method: crate::loops::preflight::ProbeMethod::Evidence,
                    usage: Usage::default(),
                    duration_ms: 0,
                },
            ],
            current_cycle: 1,
            max_cycles: 3,
            branch: Some("loop/fast-worker-reviewer-20261123-182500-a1b2".to_string()),
            base_commit: Some("deadbeef".to_string()),
            engine_pid: None,
            engine_start_ticks: None,
            caller: None,
            heartbeat_at_ms: 2000,
            last_activity_at_ms: 2000,
            started_at_ms: 1000,
            finished_at_ms: Some(3000),
            executions: vec![Execution {
                cycle: 1,
                stage_id: "implement".to_string(),
                role: "worker".to_string(),
                provider: Provider::Claude,
                model: Some("haiku".to_string()),
                model_source: Some(crate::loops::models::ModelSource::RunFlag),
                started_at_ms: 1000,
                finished_at_ms: Some(3000),
                duration_ms: 2000,
                usage: Usage {
                    turns: Some(2),
                    input_tokens: Some(100),
                    output_tokens: Some(20),
                    cache_read_tokens: Some(0),
                    cost_usd: Some(0.0100),
                },
                verdict: Some(Verdict::Pass),
                outcome: "PASS".to_string(),
                error: None,
                log_file: None,
            }],
            files: vec![],
            summary: "ok".to_string(),
        };

        let md = render_report(&state, tmp.path());
        assert!(md.contains("Pré-teste de modelos"));
        assert!(md.contains("Método"));
        assert!(md.contains("| probe |"));
        assert!(md.contains("| evidência |"));
        assert!(md.contains("$0.5000"));
        assert!(md.contains("- **Turnos:** 2\n"));
        assert!(md.contains("- **Tokens de entrada:** 100\n"));
        assert!(md.contains("- **Custo estimado:** $0.0100\n"));
        assert!(md.contains("| --use |"));
    }

    #[test]
    fn report_lists_custom_artifacts_and_the_reviews_of_rejected_cycles() {
        let tmp = TempDir::new().unwrap();
        let write = |name: &str, text: &str| std::fs::write(tmp.path().join(name), text).unwrap();
        write("input.md", "pedido");
        write("notas.md", "nota livre");
        write("critic.md", "agora sim\nVERDICT: PASS\n");
        write("c1-critic.md", "faltou teste\nVERDICT: RETRY\n");
        write("c2-critic.md", "agora sim\nVERDICT: PASS\n");
        write("report.md", "antigo");
        write("state.json", "{}");
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("x.md")).unwrap();
        assert_eq!(
            artifact_names(tmp.path())
                .into_iter()
                .filter(|n| n != "x.md")
                .collect::<Vec<_>>(),
            ["input.md", "critic.md", "notas.md", "c1-critic.md"]
        );
        assert!(read_artifact_safe(tmp.path(), "x.md").is_none());
    }

    #[test]
    fn report_redacts_extra_exact_secrets_in_fields_and_artifacts() {
        let tmp = TempDir::new().unwrap();
        let secret = "my-raw-opencode-key-value-777";
        std::fs::write(
            tmp.path().join("input.md"),
            format!("prompt using {secret}"),
        )
        .unwrap();
        let state = RunState {
            schema_version: 1,
            run_id: "r1".to_string(),
            workflow: "w".to_string(),
            status: RunStatus::Error,
            reason: Some(format!("failed with {secret}")),
            sandbox: "inherited".to_string(),
            sandbox_exposures: vec![],
            preflight: vec![],
            current_cycle: 1,
            max_cycles: 3,
            branch: None,
            base_commit: None,
            engine_pid: None,
            engine_start_ticks: None,
            caller: None,
            heartbeat_at_ms: 100,
            last_activity_at_ms: 100,
            started_at_ms: 100,
            finished_at_ms: Some(200),
            executions: vec![],
            files: vec![],
            summary: format!("summary saw {secret}"),
        };
        let md = render_report_with_secrets(&state, tmp.path(), &[secret]);
        assert!(!md.contains(secret), "secret leaked in report:\n{md}");
        assert!(md.contains("[REDACTED]"));
    }

    fn rich_state() -> RunState {
        RunState {
            schema_version: LOOP_STATE_SCHEMA_VERSION,
            run_id: "20261123-182500-a1b2".to_string(),
            workflow: "worker-reviewer-critic".to_string(),
            status: RunStatus::Pass,
            reason: None,
            sandbox: "bwrap".to_string(),
            sandbox_exposures: vec![
                "rede compartilhada com o host/container".to_string(),
                "serviços em localhost e sockets abstratos compartilhados".to_string(),
            ],
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
            heartbeat_at_ms: 5000,
            last_activity_at_ms: 5000,
            started_at_ms: 1000,
            finished_at_ms: Some(15000),
            executions: vec![
                Execution {
                    cycle: 1,
                    stage_id: "implement".to_string(),
                    role: "worker".to_string(),
                    provider: Provider::Opencode,
                    model: Some("zai/glm-5.3-flash".to_string()),
                    model_source: Some(crate::loops::models::ModelSource::ProjectStage),
                    started_at_ms: 1000,
                    finished_at_ms: Some(5000),
                    duration_ms: 4000,
                    usage: Usage {
                        turns: Some(3),
                        input_tokens: Some(18_420),
                        output_tokens: Some(500),
                        cache_read_tokens: None,
                        cost_usd: Some(0.12),
                    },
                    verdict: None,
                    outcome: "OK".to_string(),
                    error: None,
                    log_file: Some("logs/c1-01-implement.log".to_string()),
                },
                Execution {
                    cycle: 1,
                    stage_id: "audit".to_string(),
                    role: "critic".to_string(),
                    provider: Provider::Claude,
                    model: None,
                    model_source: None,
                    started_at_ms: 5000,
                    finished_at_ms: Some(10000),
                    duration_ms: 5000,
                    usage: Usage {
                        turns: Some(2),
                        input_tokens: None,
                        output_tokens: Some(200),
                        cache_read_tokens: Some(1000),
                        cost_usd: None,
                    },
                    verdict: Some(Verdict::Retry),
                    outcome: "RETRY".to_string(),
                    error: None,
                    log_file: Some("logs/c1-02-audit.log".to_string()),
                },
                Execution {
                    cycle: 2,
                    stage_id: "audit".to_string(),
                    role: "critic".to_string(),
                    provider: Provider::Claude,
                    model: None,
                    model_source: None,
                    started_at_ms: 10000,
                    finished_at_ms: Some(15000),
                    duration_ms: 5000,
                    usage: Usage {
                        turns: Some(1),
                        input_tokens: Some(500),
                        output_tokens: Some(100),
                        cache_read_tokens: Some(200),
                        cost_usd: Some(0.05),
                    },
                    verdict: Some(Verdict::Pass),
                    outcome: "PASS".to_string(),
                    error: None,
                    log_file: Some("logs/c2-03-audit.log".to_string()),
                },
            ],
            files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            summary: "Tudo corrigido com token glpat-abcdefghijklmn".to_string(),
        }
    }

    #[test]
    fn report_flags_unsandboxed_runs() {
        let tmp = TempDir::new().unwrap();
        let mut state = rich_state();
        state.sandbox = "none".to_string();
        let md = render_report(&state, tmp.path());
        assert!(
            md.contains(
                "- **Sandbox:** **SEM SANDBOX** (`none`): o agente rodou com o HOME, a rede e os poderes do seu usuário"
            ),
            "{md}"
        );
    }
}
