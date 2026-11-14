use super::hc::HcBackend;
use super::ollama::CognitiveBackend;
use super::state::{
    unix_now_secs, AutoReplyAuditEntry, DurableStateStore, LifecycleState, PendingOperatorAction,
    SupervisorMode, Urgency,
};
use super::watcher::{SupervisorRuntimeStatus, WatcherEngine};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::{Duration, Instant};

/// Serializable view of a single monitored agent for Cockpit IPC and TUI rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCardView {
    pub agent_id: String,
    pub session_name: String,
    pub lifecycle_state: LifecycleState,
    pub rolling_summary: String,
    pub audit_badge: Option<String>,
    pub auto_reply_log: Vec<AutoReplyAuditEntry>,
    pub action_card: Option<PendingOperatorAction>,
    pub last_output_preview: String,
}

/// Top-level IPC snapshot exposed to Cockpit (`--status-json` / IPC file) and TUI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopModSnapshot {
    pub pill_text: String,
    pub runtime_status: SupervisorRuntimeStatus,
    pub mode_label: String,
    pub mode: SupervisorMode,
    pub model_loaded: bool,
    #[serde(default)]
    pub llm_backend: String,
    #[serde(default)]
    pub llm_host: String,
    #[serde(default)]
    pub llm_model: String,
    #[serde(default)]
    pub num_gpu: u32,
    #[serde(default)]
    pub llm_connected: bool,
    #[serde(default)]
    pub model_installed: bool,
    #[serde(default)]
    pub llm_status_detail: String,
    pub agents: Vec<AgentCardView>,
}

/// Builds a `DesktopModSnapshot` from the live `WatcherEngine`.
pub fn build_desktop_mod_snapshot<H: HcBackend, C: CognitiveBackend>(
    engine: &WatcherEngine<H, C>,
) -> DesktopModSnapshot {
    let mut snap = build_desktop_mod_snapshot_from_parts(
        &engine.runtime_status,
        engine.model_loaded,
        &engine.store,
    );
    snap.llm_backend = engine.config.llm_backend.as_str().to_string();
    snap.llm_host = engine.config.effective_primary_host();
    snap.llm_model = engine.config.ollama_model.clone();
    snap.num_gpu = engine.config.effective_num_gpu();
    snap
}

/// Enriches a `DesktopModSnapshot` with live LLM connectivity and model availability status.
pub fn enrich_snapshot_with_llm_health(
    snap: &mut DesktopModSnapshot,
    config: &super::config::SupervisorConfig,
) {
    let health = super::ollama::probe_llm_health(config);
    snap.llm_backend = health.backend;
    snap.llm_host = health.host;
    snap.llm_model = health.model;
    snap.num_gpu = health.num_gpu;
    snap.llm_connected = health.connected;
    snap.model_installed = health.model_installed;
    snap.model_loaded = snap.model_loaded || health.model_loaded;
    snap.llm_status_detail = health.detail;
}

/// Builds a `DesktopModSnapshot` from decoupled state components.
pub fn build_desktop_mod_snapshot_from_parts(
    runtime_status: &SupervisorRuntimeStatus,
    model_loaded: bool,
    store: &DurableStateStore,
) -> DesktopModSnapshot {
    let agents = store
        .agents
        .values()
        .map(|agent| {
            let audit_badge =
                if agent.consecutive_auto_replies > 0 || !agent.auto_reply_log.is_empty() {
                    Some(format!(
                        "Auto-assisted ({}/{})",
                        agent.consecutive_auto_replies, agent.max_auto_replies
                    ))
                } else {
                    None
                };

            AgentCardView {
                agent_id: agent.agent_id.clone(),
                session_name: agent.session_name.clone(),
                lifecycle_state: agent.lifecycle_state,
                rolling_summary: agent.rolling_summary.clone(),
                audit_badge,
                auto_reply_log: agent.auto_reply_log.clone(),
                action_card: agent.pending_operator_action.clone(),
                last_output_preview: agent.last_output_preview.clone(),
            }
        })
        .collect();

    DesktopModSnapshot {
        pill_text: runtime_status.pill_text(),
        runtime_status: runtime_status.clone(),
        mode_label: store.mode.label().to_string(),
        mode: store.mode,
        model_loaded,
        llm_backend: String::new(),
        llm_host: String::new(),
        llm_model: String::new(),
        num_gpu: 0,
        llm_connected: true,
        model_installed: true,
        llm_status_detail: String::new(),
        agents,
    }
}

/// Interactive state for the Ratatui Supervisor TUI.
#[derive(Debug, Default)]
pub struct SupervisorTuiApp {
    pub selected_index: usize,
    pub show_audit_log: bool,
    pub editing_command: bool,
    pub command_draft: String,
    command_draft_agent_id: Option<String>,
    pub status_message: Option<String>,
}

impl SupervisorTuiApp {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensures `selected_index` is in bounds and syncs `command_draft` with the selected agent's Action Card.
    pub fn sync_with_snapshot(&mut self, snapshot: &DesktopModSnapshot) {
        if snapshot.agents.is_empty() {
            self.selected_index = 0;
            self.editing_command = false;
            self.command_draft.clear();
            self.command_draft_agent_id = None;
            return;
        }

        if self.selected_index >= snapshot.agents.len() {
            self.selected_index = snapshot.agents.len() - 1;
            self.editing_command = false;
        }

        let agent = &snapshot.agents[self.selected_index];
        match &agent.action_card {
            Some(card) => {
                let agent_changed = self.command_draft_agent_id.as_deref() != Some(&agent.agent_id);
                if agent_changed && !self.editing_command {
                    self.command_draft = card.hc_command_payload.clone();
                    self.command_draft_agent_id = Some(agent.agent_id.clone());
                } else if self.command_draft_agent_id.is_none() {
                    self.command_draft = card.hc_command_payload.clone();
                    self.command_draft_agent_id = Some(agent.agent_id.clone());
                }
            }
            None => {
                self.editing_command = false;
                self.command_draft.clear();
                self.command_draft_agent_id = None;
            }
        }
    }

    pub fn select_next(&mut self, len: usize) {
        if len == 0 {
            self.selected_index = 0;
        } else {
            self.selected_index = (self.selected_index + 1) % len;
            self.editing_command = false;
            self.command_draft_agent_id = None;
        }
    }

    pub fn select_prev(&mut self, len: usize) {
        if len == 0 {
            self.selected_index = 0;
        } else if self.selected_index == 0 {
            self.selected_index = len - 1;
            self.editing_command = false;
            self.command_draft_agent_id = None;
        } else {
            self.selected_index -= 1;
            self.editing_command = false;
            self.command_draft_agent_id = None;
        }
    }

    pub fn selected_agent<'a>(&self, snapshot: &'a DesktopModSnapshot) -> Option<&'a AgentCardView> {
        snapshot.agents.get(self.selected_index)
    }

    /// Renders the complete Cockpit Durable Supervisor TUI into a Ratatui frame.
    pub fn render(&mut self, frame: &mut Frame, snapshot: &DesktopModSnapshot, allow_gpu: bool) {
        self.sync_with_snapshot(snapshot);

        let root_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(frame.area());

        render_header(frame, root_chunks[0], snapshot, allow_gpu);

        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(root_chunks[1]);

        self.render_agent_list(frame, body_chunks[0], snapshot);
        self.render_detail_panel(frame, body_chunks[1], snapshot);
        self.render_footer(frame, root_chunks[2]);
    }

    fn render_agent_list(&self, frame: &mut Frame, area: Rect, snapshot: &DesktopModSnapshot) {
        let block = Block::default()
            .title(format!(" Monitored Agents ({}) ", snapshot.agents.len()))
            .borders(Borders::ALL);

        if snapshot.agents.is_empty() {
            let empty = Paragraph::new("No active agent sessions detected by hc.")
                .style(Style::default().fg(Color::DarkGray))
                .block(block)
                .wrap(Wrap { trim: true });
            frame.render_widget(empty, area);
            return;
        }

        let items: Vec<ListItem> = snapshot
            .agents
            .iter()
            .map(|agent| {
                let state_color = lifecycle_color(agent.lifecycle_state);
                let mut header_spans = vec![
                    Span::styled(
                        format!("{} ", agent.agent_id),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("({}) ", agent.session_name),
                        Style::default().fg(Color::Gray),
                    ),
                    Span::styled(
                        format!("[{}]", agent.lifecycle_state.as_str()),
                        Style::default().fg(state_color),
                    ),
                ];

                if agent.action_card.is_some() {
                    header_spans.push(Span::styled(
                        " [ALERT]",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }

                let mut lines = vec![
                    Line::from(header_spans),
                    Line::from(Span::styled(
                        format!("  {}", agent.rolling_summary),
                        Style::default().fg(Color::White),
                    )),
                ];

                if let Some(ref badge) = agent.audit_badge {
                    lines.push(Line::from(Span::styled(
                        format!("  {badge}"),
                        Style::default().fg(Color::Cyan),
                    )));
                }

                ListItem::new(lines)
            })
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");

        let mut list_state = ListState::default();
        list_state.select(Some(self.selected_index));
        frame.render_stateful_widget(list, area, &mut list_state);
    }

    fn render_detail_panel(&self, frame: &mut Frame, area: Rect, snapshot: &DesktopModSnapshot) {
        let Some(agent) = self.selected_agent(snapshot) else {
            let block = Block::default()
                .title(" Session Details ")
                .borders(Borders::ALL);
            let msg = Paragraph::new("Select an agent session to inspect output and alerts.")
                .style(Style::default().fg(Color::DarkGray))
                .block(block);
            frame.render_widget(msg, area);
            return;
        };

        if let Some(ref alert) = agent.action_card {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(11), Constraint::Min(6)])
                .split(area);

            self.render_action_card(frame, chunks[0], agent, alert);
            self.render_secondary_pane(frame, chunks[1], agent);
        } else {
            self.render_secondary_pane(frame, area, agent);
        }
    }

    fn render_action_card(
        &self,
        frame: &mut Frame,
        area: Rect,
        agent: &AgentCardView,
        alert: &PendingOperatorAction,
    ) {
        let urgency_color = match alert.urgency {
            Urgency::Critical => Color::Red,
            Urgency::Normal => Color::Yellow,
            Urgency::Low => Color::Blue,
        };

        let block = Block::default()
            .title(Line::from(vec![
                Span::raw(" Action Card: "),
                Span::styled(
                    format!("[{}]", alert.urgency.display_badge()),
                    Style::default()
                        .fg(urgency_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " {} ({}) ",
                    alert.detected_state.as_str(),
                    agent.agent_id
                )),
            ]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(urgency_color));

        let edit_label = if self.editing_command {
            "[EDITING: Enter=Save, Esc=Cancel]"
        } else {
            "[E] Edit"
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("Motivo: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(alert.reason_for_operator.clone()),
            ]),
            Line::from(vec![
                Span::styled("Resumo: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(alert.updated_summary.clone()),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Comando: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    self.command_draft.clone(),
                    if self.editing_command {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Green)
                    },
                ),
                Span::styled(
                    format!("  {edit_label}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "[Aprovar e Enviar]",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" (a)    "),
                Span::styled(
                    "[Ignorar]",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" (i)"),
            ]),
        ];

        let card = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        frame.render_widget(card, area);
    }

    fn render_secondary_pane(&self, frame: &mut Frame, area: Rect, agent: &AgentCardView) {
        if self.show_audit_log {
            let block = Block::default()
                .title(format!(
                    " Auto-Reply Audit Log ({}) [L to close] ",
                    agent.auto_reply_log.len()
                ))
                .borders(Borders::ALL);

            if agent.auto_reply_log.is_empty() {
                let empty = Paragraph::new("No auto-replies recorded for this agent yet.")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(block);
                frame.render_widget(empty, area);
                return;
            }

            let lines: Vec<Line> = agent
                .auto_reply_log
                .iter()
                .rev()
                .map(|entry| {
                    Line::from(vec![
                        Span::styled(
                            format!("[{}] ", entry.timestamp),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("sent {:?} ", entry.payload),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::raw(format!("— {}", entry.summary)),
                    ])
                })
                .collect();

            let log_widget = Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: true });
            frame.render_widget(log_widget, area);
        } else {
            let title = match &agent.audit_badge {
                Some(badge) => format!(" Output Preview ({}) — {} ", agent.agent_id, badge),
                None => format!(" Output Preview ({}) ", agent.agent_id),
            };
            let block = Block::default().title(title).borders(Borders::ALL);
            let preview_text = if agent.last_output_preview.trim().is_empty() {
                "No terminal output captured yet."
            } else {
                agent.last_output_preview.as_str()
            };
            let preview = Paragraph::new(preview_text)
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(preview, area);
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let help = if self.editing_command {
            "Editing Command: type text | Backspace | Enter=Confirm | Esc=Cancel"
        } else {
            "Keys: [↑/↓ j/k] Select | [m] Mode | [e] Edit | [a] Approve | [i] Ignore | [t] Tick | [u] Unload | [l] Audit | [q] Quit"
        };

        let content = match &self.status_message {
            Some(status) => format!("{status}  |  {help}"),
            None => help.to_string(),
        };

        let footer = Paragraph::new(content)
            .style(Style::default().fg(Color::Gray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, area);
    }
}

fn render_header(frame: &mut Frame, area: Rect, snapshot: &DesktopModSnapshot, allow_gpu: bool) {
    let pill_color = match snapshot.runtime_status {
        SupervisorRuntimeStatus::Idle => Color::DarkGray,
        SupervisorRuntimeStatus::Watching(_) => Color::Green,
        SupervisorRuntimeStatus::Evaluating(_) => Color::Yellow,
    };

    let mode_color = match snapshot.mode {
        SupervisorMode::SemiAuto => Color::Cyan,
        SupervisorMode::AlertOnly => Color::Yellow,
        SupervisorMode::Paused => Color::Red,
    };

    let vram_label = if allow_gpu {
        "GPU enabled"
    } else {
        "CPU-only (num_gpu: 0)"
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" ● {} ", snapshot.pill_text),
            Style::default()
                .fg(pill_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | Mode: "),
        Span::styled(
            snapshot.mode_label.clone(),
            Style::default()
                .fg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" [M] | VRAM: "),
        Span::styled(vram_label, Style::default().fg(Color::Gray)),
    ]);

    let header = Paragraph::new(line).block(
        Block::default()
            .title(" Cockpit Durable Supervisor ")
            .borders(Borders::ALL),
    );
    frame.render_widget(header, area);
}

fn lifecycle_color(state: LifecycleState) -> Color {
    match state {
        LifecycleState::Working => Color::Green,
        LifecycleState::WaitingConfirmation => Color::Yellow,
        LifecycleState::StuckLoop => Color::Red,
        LifecycleState::Error => Color::Red,
        LifecycleState::Completed => Color::Blue,
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

/// Runs the interactive Ratatui + Crossterm TUI event loop for the Cockpit Durable Supervisor.
pub fn run_tui<H: HcBackend, C: CognitiveBackend>(
    engine: &mut WatcherEngine<H, C>,
) -> Result<()> {
    enable_raw_mode().context("Failed to enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to initialize Ratatui terminal")?;

    let result = run_tui_loop(&mut terminal, engine);
    let _ = engine.shutdown_unload();
    result
}

fn run_tui_loop<B: Backend, H: HcBackend, C: CognitiveBackend>(
    terminal: &mut Terminal<B>,
    engine: &mut WatcherEngine<H, C>,
) -> Result<()> {
    let mut app = SupervisorTuiApp::new();
    let _ = engine.tick(unix_now_secs());
    let poll_interval = Duration::from_secs(engine.config.poll_interval_secs.max(1));
    let mut last_tick = Instant::now();

    loop {
        let snapshot = build_desktop_mod_snapshot(engine);
        let allow_gpu = engine.config.allow_gpu;
        terminal.draw(|frame| app.render(frame, &snapshot, allow_gpu))?;

        if event::poll(Duration::from_millis(250)).context("Failed to poll terminal events")? {
            if let Event::Key(key) = event::read().context("Failed to read terminal event")? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if handle_key_event(key, &mut app, &snapshot, engine)? {
                    break;
                }
            }
        }

        if last_tick.elapsed() >= poll_interval {
            if let Err(err) = engine.tick(unix_now_secs()) {
                app.status_message = Some(format!("Tick error: {err}"));
            }
            last_tick = Instant::now();
        }
    }

    Ok(())
}

fn handle_key_event<H: HcBackend, C: CognitiveBackend>(
    key: KeyEvent,
    app: &mut SupervisorTuiApp,
    snapshot: &DesktopModSnapshot,
    engine: &mut WatcherEngine<H, C>,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if app.editing_command {
        match key.code {
            KeyCode::Esc => {
                app.editing_command = false;
                if let Some(agent) = app.selected_agent(snapshot) {
                    if let Some(ref card) = agent.action_card {
                        app.command_draft = card.hc_command_payload.clone();
                    }
                }
                app.status_message = Some("Command edit cancelled.".to_string());
            }
            KeyCode::Enter => {
                app.editing_command = false;
                app.status_message = Some("Command draft updated.".to_string());
            }
            KeyCode::Backspace => {
                app.command_draft.pop();
            }
            KeyCode::Char(c) => {
                app.command_draft.push(c);
            }
            _ => {}
        }
        return Ok(false);
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(true),
        KeyCode::Up | KeyCode::Char('k') => app.select_prev(snapshot.agents.len()),
        KeyCode::Down | KeyCode::Char('j') => app.select_next(snapshot.agents.len()),
        KeyCode::Char('m') | KeyCode::Char('M') => match engine.cycle_mode() {
            Ok(mode) => app.status_message = Some(format!("Mode: {}", mode.label())),
            Err(err) => app.status_message = Some(format!("Failed to cycle mode: {err}")),
        },
        KeyCode::Char('l') | KeyCode::Char('L') => {
            app.show_audit_log = !app.show_audit_log;
        }
        KeyCode::Char('t') | KeyCode::Char('T') | KeyCode::Char('r') | KeyCode::Char('R') => {
            match engine.tick(unix_now_secs()) {
                Ok(_) => app.status_message = Some("Tick completed.".to_string()),
                Err(err) => app.status_message = Some(format!("Tick failed: {err}")),
            }
        }
        KeyCode::Char('u') | KeyCode::Char('U') => match engine.shutdown_unload() {
            Ok(()) => app.status_message = Some("LLM model unloaded.".to_string()),
            Err(err) => app.status_message = Some(format!("Unload failed: {err}")),
        },
        KeyCode::Char('e') | KeyCode::Char('E') => {
            if let Some(agent) = app.selected_agent(snapshot) {
                if agent.action_card.is_some() {
                    app.editing_command = true;
                    app.status_message = Some("Editing command preview...".to_string());
                }
            }
        }
        KeyCode::Char('a') | KeyCode::Char('A') => {
            if let Some(agent) = app.selected_agent(snapshot) {
                if agent.action_card.is_some() {
                    let payload = app.command_draft.clone();
                    match engine.approve_operator_action(&agent.agent_id, &payload, unix_now_secs())
                    {
                        Ok(()) => {
                            app.status_message =
                                Some(format!("Approved & sent to {}", agent.agent_id));
                        }
                        Err(err) => {
                            app.status_message = Some(format!("Approve failed: {err}"));
                        }
                    }
                }
            }
        }
        KeyCode::Char('i') | KeyCode::Char('I') => {
            if let Some(agent) = app.selected_agent(snapshot) {
                if agent.action_card.is_some() {
                    match engine.ignore_operator_action(&agent.agent_id) {
                        Ok(()) => {
                            app.status_message =
                                Some(format!("Ignored alert for {}", agent.agent_id));
                        }
                        Err(err) => {
                            app.status_message = Some(format!("Ignore failed: {err}"));
                        }
                    }
                }
            }
        }
        _ => {}
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::state::{AgentDurableState, AutoReplyAuditEntry, DurableStateStore};
    use ratatui::backend::TestBackend;

    fn buffer_to_text(backend: &TestBackend) -> String {
        let buf = backend.buffer();
        let mut lines = Vec::with_capacity(buf.area.height as usize);
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            lines.push(row);
        }
        lines.join("\n")
    }

    #[test]
    fn test_build_desktop_mod_snapshot_idle_and_watching_with_audit_and_action_card() {
        let mut store = DurableStateStore::default();

        let idle_snap =
            build_desktop_mod_snapshot_from_parts(&SupervisorRuntimeStatus::Idle, false, &store);
        assert_eq!(idle_snap.pill_text, "Supervisor: Idle (0 MB)");
        assert_eq!(idle_snap.mode_label, "Semi-Auto (Whitelist)");
        assert!(!idle_snap.model_loaded);
        assert!(idle_snap.agents.is_empty());

        let mut agent1 = AgentDurableState::new("worker-01", "refactor-auth", 2, 1_700_000_000);
        agent1.consecutive_auto_replies = 1;
        agent1.max_auto_replies = 2;
        agent1.auto_reply_log.push(AutoReplyAuditEntry {
            timestamp: 1_700_000_010,
            payload: "y".to_string(),
            summary: "Confirmou inspeção de leitura.".to_string(),
        });
        agent1.pending_operator_action = Some(PendingOperatorAction {
            urgency: Urgency::Critical,
            detected_state: LifecycleState::WaitingConfirmation,
            reason_for_operator: "Agente solicitou git push --force.".to_string(),
            updated_summary: "Aguardando aprovação do operador.".to_string(),
            hc_command_payload: "n".to_string(),
            triggered_at: 1_700_000_020,
        });
        agent1.last_output_preview = "Run git push --force? [y/N]".to_string();

        let agent2 = AgentDurableState::new("worker-02", "fix-tests", 2, 1_700_000_005);
        store.agents.insert(agent1.agent_id.clone(), agent1);
        store.agents.insert(agent2.agent_id.clone(), agent2);

        let watching_snap = build_desktop_mod_snapshot_from_parts(
            &SupervisorRuntimeStatus::Watching(2),
            true,
            &store,
        );
        assert_eq!(watching_snap.pill_text, "Supervisor: Watching (2)");
        assert!(watching_snap.model_loaded);
        assert_eq!(watching_snap.agents.len(), 2);

        let card1 = &watching_snap.agents[0];
        assert_eq!(card1.agent_id, "worker-01");
        assert_eq!(card1.audit_badge, Some("Auto-assisted (1/2)".to_string()));
        assert!(card1.action_card.is_some());
        assert_eq!(
            card1.action_card.as_ref().unwrap().hc_command_payload,
            "n"
        );

        let card2 = &watching_snap.agents[1];
        assert_eq!(card2.audit_badge, None);
        assert!(card2.action_card.is_none());
    }

    #[test]
    fn test_tui_renders_pill_mode_audit_badge_and_action_card_buttons() {
        let mut store = DurableStateStore::default();
        let mut agent = AgentDurableState::new("worker-01", "refactor-auth", 2, 1_700_000_000);
        agent.consecutive_auto_replies = 1;
        agent.max_auto_replies = 2;
        agent.pending_operator_action = Some(PendingOperatorAction {
            urgency: Urgency::Normal,
            detected_state: LifecycleState::WaitingConfirmation,
            reason_for_operator: "Solicitou confirmação para continuar.".to_string(),
            updated_summary: "Aguardando confirmação do operador.".to_string(),
            hc_command_payload: "cargo test".to_string(),
            triggered_at: 1_700_000_020,
        });
        agent.last_output_preview = "Proceed with cargo test? [y/N]".to_string();
        store.agents.insert(agent.agent_id.clone(), agent);

        let snapshot = build_desktop_mod_snapshot_from_parts(
            &SupervisorRuntimeStatus::Watching(1),
            true,
            &store,
        );

        let backend = TestBackend::new(100, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = SupervisorTuiApp::new();

        terminal
            .draw(|frame| app.render(frame, &snapshot, false))
            .unwrap();

        let rendered = buffer_to_text(terminal.backend());
        assert!(
            rendered.contains("Cockpit Durable Supervisor"),
            "Missing header title in:\n{rendered}"
        );
        assert!(
            rendered.contains("Supervisor: Watching"),
            "Missing pill text in:\n{rendered}"
        );
        assert!(
            rendered.contains("Semi-Auto (Whitelist)"),
            "Missing mode label in:\n{rendered}"
        );
        assert!(
            rendered.contains("Auto-assisted (1/2)"),
            "Missing audit badge in:\n{rendered}"
        );
        assert!(
            rendered.contains("[Aprovar e Enviar]"),
            "Missing approve button in:\n{rendered}"
        );
        assert!(
            rendered.contains("[Ignorar]"),
            "Missing ignore button in:\n{rendered}"
        );
    }
}
