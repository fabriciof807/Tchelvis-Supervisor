use crate::config::CockpitPaths;
use crate::correlate::{resolve_git_workspace, should_include_terminal};
use crate::herdr::HcTerminal;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SCANNED_PROJECTS: usize = 25;
const MAX_GIT_ENRICHED_PROJECTS: usize = 12;

fn default_store_version() -> u32 {
    1
}

fn default_next_id() -> u64 {
    1
}

pub fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    High,
    #[default]
    Normal,
    Low,
}

impl TaskPriority {
    pub fn from_str_loose(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" | "h" | "p1" | "urgent" => Self::High,
            "low" | "l" | "p3" => Self::Low,
            _ => Self::Normal,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Normal => "normal",
            Self::Low => "low",
        }
    }

    fn sort_rank(&self) -> u8 {
        match self {
            Self::High => 0,
            Self::Normal => 1,
            Self::Low => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub priority: TaskPriority,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub completed_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyTaskStore {
    #[serde(default = "default_store_version")]
    pub version: u32,
    #[serde(default = "default_next_id")]
    pub next_id: u64,
    #[serde(default)]
    pub tasks: Vec<DailyTask>,
}

impl Default for DailyTaskStore {
    fn default() -> Self {
        Self {
            version: 1,
            next_id: 1,
            tasks: Vec::new(),
        }
    }
}

impl DailyTaskStore {
    pub fn default_path(paths: &CockpitPaths) -> PathBuf {
        paths.state_dir.join("daily_tasks.json")
    }

    pub fn load_or_init(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed reading daily tasks from {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let mut store: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed parsing daily tasks JSON at {}", path.display()))?;
        if store.version == 0 {
            store.version = 1;
        }
        if store.next_id == 0 {
            store.next_id = 1;
        }
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed creating daily tasks directory {}", parent.display())
            })?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, json).with_context(|| {
            format!("failed writing temporary daily tasks file {}", tmp_path.display())
        })?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("failed saving daily tasks to {}", path.display()))?;
        Ok(())
    }

    pub fn add_task(
        &mut self,
        title: &str,
        project: Option<String>,
        priority: TaskPriority,
        now: u64,
    ) -> DailyTask {
        let mut id = format!("t-{}", self.next_id);
        self.next_id += 1;
        while self.tasks.iter().any(|t| t.id == id) {
            id = format!("t-{}", self.next_id);
            self.next_id += 1;
        }

        let normalized_project = project
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());

        let task = DailyTask {
            id,
            title: title.trim().to_string(),
            project: normalized_project,
            priority,
            done: false,
            created_at: now,
            completed_at: None,
        };
        self.tasks.push(task.clone());
        task
    }

    pub fn toggle_task(&mut self, id: &str, now: u64) -> Option<DailyTask> {
        let target = id.trim();
        let task = self.tasks.iter_mut().find(|t| t.id == target)?;
        task.done = !task.done;
        task.completed_at = if task.done { Some(now) } else { None };
        Some(task.clone())
    }

    pub fn remove_task(&mut self, id: &str) -> bool {
        let target = id.trim();
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != target);
        self.tasks.len() < before
    }

    pub fn clear_done(&mut self) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|t| !t.done);
        before.saturating_sub(self.tasks.len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub name: String,
    pub path: String,
    pub git_branch: Option<String>,
    pub git_dirty_count: usize,
    pub last_commit_subject: Option<String>,
    pub active_agents: usize,
    pub blocked_agents: usize,
    pub total_terminals: usize,
    pub open_tasks: usize,
    pub status_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeSummaryStats {
    pub total_projects: usize,
    pub active_projects: usize,
    pub running_agents: usize,
    pub blocked_agents: usize,
    pub tasks_total: usize,
    pub tasks_done: usize,
    pub tasks_pending: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeSnapshot {
    pub schema_version: u32,
    pub updated_at: u64,
    pub summary: HomeSummaryStats,
    pub tasks: Vec<DailyTask>,
    pub projects: Vec<ProjectSummary>,
}

#[derive(Debug, Deserialize)]
struct HcLsOutput {
    #[serde(default)]
    terminals: Vec<HcTerminal>,
}

#[derive(Debug, Default)]
struct ProjectAccumulator {
    path: PathBuf,
    active_agents: usize,
    blocked_agents: usize,
    total_terminals: usize,
}

fn fetch_live_hc_terminals() -> Vec<HcTerminal> {
    let output = Command::new("hc")
        .args(["ls", "--json"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(parsed) = serde_json::from_slice::<HcLsOutput>(&output.stdout) else {
        return Vec::new();
    };
    parsed.terminals
}

fn canonical_or_clean(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn status_rank(label: &str) -> u8 {
    match label {
        "blocked" => 0,
        "active" => 1,
        "open_tasks" => 2,
        _ => 3,
    }
}

fn compute_status_label(active_agents: usize, blocked_agents: usize, open_tasks: usize) -> String {
    if blocked_agents > 0 {
        "blocked".to_string()
    } else if active_agents > 0 {
        "active".to_string()
    } else if open_tasks > 0 {
        "open_tasks".to_string()
    } else {
        "idle".to_string()
    }
}

fn read_git_dirty_and_subject(path: &Path) -> (usize, Option<String>) {
    let dirty_count = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain", "-uno"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0);

    let last_commit_subject = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["log", "-1", "--pretty=%s"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        });

    (dirty_count, last_commit_subject)
}

fn discover_git_subdirs(home_dir: &Path) -> Vec<PathBuf> {
    let scan_roots = [
        home_dir.join("Repos"),
        home_dir.join("projetos"),
        home_dir.join(".t3/projects"),
    ];
    let mut found = Vec::new();
    let mut seen = HashSet::new();

    for root in scan_roots {
        if found.len() >= MAX_SCANNED_PROJECTS {
            break;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if found.len() >= MAX_SCANNED_PROJECTS {
                break;
            }
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let dot_git = dir.join(".git");
            if dot_git.is_dir() || dot_git.is_file() {
                let canon = canonical_or_clean(&dir);
                if seen.insert(canon.clone()) {
                    found.push(canon);
                }
            }
        }
    }

    found
}

pub fn build_home_snapshot(paths: &CockpitPaths) -> Result<HomeSnapshot> {
    let terminals = fetch_live_hc_terminals();
    build_home_snapshot_with_terminals(paths, &terminals, unix_now_secs())
}

pub fn build_home_snapshot_with_terminals(
    paths: &CockpitPaths,
    terminals: &[HcTerminal],
    now: u64,
) -> Result<HomeSnapshot> {
    let store = DailyTaskStore::load_or_init(&DailyTaskStore::default_path(paths))?;
    let mut tasks = store.tasks;
    tasks.sort_by(|a, b| {
        a.done
            .cmp(&b.done)
            .then_with(|| a.priority.sort_rank().cmp(&b.priority.sort_rank()))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    let mut project_map: BTreeMap<PathBuf, ProjectAccumulator> = BTreeMap::new();

    for term in terminals {
        if term.state.eq_ignore_ascii_case("finished") {
            continue;
        }
        let cwd = term.effective_cwd();
        let ws = resolve_git_workspace(&cwd);
        let Some(project_dir_str) = ws.worktree_path.or(ws.repo_root) else {
            continue;
        };
        let project_path = canonical_or_clean(Path::new(&project_dir_str));
        let entry = project_map
            .entry(project_path.clone())
            .or_insert_with(|| ProjectAccumulator {
                path: project_path,
                ..Default::default()
            });
        entry.total_terminals += 1;

        if should_include_terminal(term, false) {
            entry.active_agents += 1;
            let is_blocked = term
                .status
                .as_ref()
                .is_some_and(|st| st.state.trim().eq_ignore_ascii_case("blocked"));
            if is_blocked {
                entry.blocked_agents += 1;
            }
        }
    }

    for dir in discover_git_subdirs(&paths.home_dir) {
        project_map.entry(dir.clone()).or_insert_with(|| ProjectAccumulator {
            path: dir,
            ..Default::default()
        });
    }

    let mut projects: Vec<ProjectSummary> = project_map
        .into_values()
        .map(|acc| {
            let name = acc
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| acc.path.to_string_lossy().to_string());
            let path_str = acc.path.to_string_lossy().to_string();
            let ws = resolve_git_workspace(&path_str);
            let open_tasks = tasks
                .iter()
                .filter(|t| {
                    !t.done
                        && t.project
                            .as_deref()
                            .is_some_and(|p| p.trim().eq_ignore_ascii_case(&name))
                })
                .count();
            let status_label =
                compute_status_label(acc.active_agents, acc.blocked_agents, open_tasks);

            ProjectSummary {
                name,
                path: path_str,
                git_branch: ws.git_branch,
                git_dirty_count: 0,
                last_commit_subject: None,
                active_agents: acc.active_agents,
                blocked_agents: acc.blocked_agents,
                total_terminals: acc.total_terminals,
                open_tasks,
                status_label,
            }
        })
        .collect();

    projects.sort_by(|a, b| {
        status_rank(&a.status_label)
            .cmp(&status_rank(&b.status_label))
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    for project in projects.iter_mut().take(MAX_GIT_ENRICHED_PROJECTS) {
        let (dirty, subject) = read_git_dirty_and_subject(Path::new(&project.path));
        project.git_dirty_count = dirty;
        project.last_commit_subject = subject;
    }

    let tasks_total = tasks.len();
    let tasks_done = tasks.iter().filter(|t| t.done).count();
    let tasks_pending = tasks_total.saturating_sub(tasks_done);
    let active_projects = projects
        .iter()
        .filter(|p| p.active_agents > 0 || p.blocked_agents > 0)
        .count();
    let running_agents = projects.iter().map(|p| p.active_agents).sum();
    let blocked_agents = projects.iter().map(|p| p.blocked_agents).sum();

    Ok(HomeSnapshot {
        schema_version: 1,
        updated_at: now,
        summary: HomeSummaryStats {
            total_projects: projects.len(),
            active_projects,
            running_agents,
            blocked_agents,
            tasks_total,
            tasks_done,
            tasks_pending,
        },
        tasks,
        projects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::{HcForeground, HcStatus};
    use tempfile::TempDir;

    fn test_paths(tmp: &TempDir) -> CockpitPaths {
        let home_dir = tmp.path().to_path_buf();
        CockpitPaths {
            config_dir: home_dir.join(".config/cockpit"),
            data_dir: home_dir.join(".local/share/cockpit"),
            state_dir: home_dir.join(".local/state/cockpit"),
            runtime_dir: home_dir.join(".local/state/cockpit/run"),
            home_dir,
        }
    }

    #[test]
    fn test_task_priority_loose_parsing() {
        assert_eq!(TaskPriority::from_str_loose("high"), TaskPriority::High);
        assert_eq!(TaskPriority::from_str_loose("HIGH"), TaskPriority::High);
        assert_eq!(TaskPriority::from_str_loose("p1"), TaskPriority::High);
        assert_eq!(TaskPriority::from_str_loose("low"), TaskPriority::Low);
        assert_eq!(TaskPriority::from_str_loose("P3"), TaskPriority::Low);
        assert_eq!(TaskPriority::from_str_loose("normal"), TaskPriority::Normal);
        assert_eq!(TaskPriority::from_str_loose("unknown"), TaskPriority::Normal);
        assert_eq!(TaskPriority::High.as_str(), "high");
        assert_eq!(TaskPriority::Normal.as_str(), "normal");
        assert_eq!(TaskPriority::Low.as_str(), "low");
    }

    #[test]
    fn test_daily_task_store_crud_and_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daily_tasks.json");

        let mut store = DailyTaskStore::load_or_init(&path).unwrap();
        assert_eq!(store.version, 1);
        assert_eq!(store.next_id, 1);
        assert!(store.tasks.is_empty());

        let t1 = store.add_task("Ship home view", Some("cockpit".to_string()), TaskPriority::High, 100);
        let t2 = store.add_task("Review PR", None, TaskPriority::Normal, 200);
        assert_eq!(t1.id, "t-1");
        assert_eq!(t2.id, "t-2");
        assert!(!t1.done);
        assert_eq!(t1.completed_at, None);

        let toggled = store.toggle_task("t-1", 300).unwrap();
        assert!(toggled.done);
        assert_eq!(toggled.completed_at, Some(300));

        store.save(&path).unwrap();
        let reloaded = DailyTaskStore::load_or_init(&path).unwrap();
        assert_eq!(reloaded, store);

        let mut reloaded = reloaded;
        let cleared = reloaded.clear_done();
        assert_eq!(cleared, 1);
        assert_eq!(reloaded.tasks.len(), 1);
        assert_eq!(reloaded.tasks[0].id, "t-2");

        assert!(reloaded.remove_task("t-2"));
        assert!(!reloaded.remove_task("t-999"));
        assert!(reloaded.tasks.is_empty());
    }

    #[test]
    fn test_build_home_snapshot_discovers_projects_and_sorts() {
        let tmp = TempDir::new().unwrap();
        let paths = test_paths(&tmp);
        fs::create_dir_all(&paths.state_dir).unwrap();

        let alpha = paths.home_dir.join("Repos/alpha");
        let beta = paths.home_dir.join("projetos/beta");
        let gamma = paths.home_dir.join(".t3/projects/gamma");
        for (dir, branch) in [(&alpha, "main"), (&beta, "feat/ui"), (&gamma, "dev")] {
            fs::create_dir_all(dir.join(".git")).unwrap();
            fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        }

        let mut store = DailyTaskStore::default();
        store.add_task("Fix alpha bug", Some("alpha".to_string()), TaskPriority::High, 10);
        store.add_task("Done task", Some("gamma".to_string()), TaskPriority::Low, 20);
        store.toggle_task("t-2", 25);
        store.add_task("Write tests", Some("beta".to_string()), TaskPriority::Normal, 30);
        store.save(&DailyTaskStore::default_path(&paths)).unwrap();

        let terminals = vec![HcTerminal {
            address: "t1".to_string(),
            state: "live".to_string(),
            agent: Some("claude".to_string()),
            cwd: Some(gamma.to_string_lossy().to_string()),
            foreground: Some(HcForeground {
                name: Some("claude".to_string()),
                cwd: Some(gamma.to_string_lossy().to_string()),
                ..Default::default()
            }),
            status: Some(HcStatus {
                state: "blocked".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let snap = build_home_snapshot_with_terminals(&paths, &terminals, 999).unwrap();
        assert_eq!(snap.schema_version, 1);
        assert_eq!(snap.updated_at, 999);
        assert_eq!(snap.summary.total_projects, 3);
        assert_eq!(snap.summary.active_projects, 1);
        assert_eq!(snap.summary.running_agents, 1);
        assert_eq!(snap.summary.blocked_agents, 1);
        assert_eq!(snap.summary.tasks_total, 3);
        assert_eq!(snap.summary.tasks_done, 1);
        assert_eq!(snap.summary.tasks_pending, 2);

        // Tasks: pending High first, then pending Normal, then done Low
        assert_eq!(snap.tasks[0].id, "t-1");
        assert_eq!(snap.tasks[1].id, "t-3");
        assert_eq!(snap.tasks[2].id, "t-2");

        // Projects: blocked ("gamma") first, then open_tasks ("alpha", "beta" alphabetically)
        assert_eq!(snap.projects[0].name, "gamma");
        assert_eq!(snap.projects[0].status_label, "blocked");
        assert_eq!(snap.projects[0].git_branch.as_deref(), Some("dev"));
        assert_eq!(snap.projects[1].name, "alpha");
        assert_eq!(snap.projects[1].status_label, "open_tasks");
        assert_eq!(snap.projects[1].open_tasks, 1);
        assert_eq!(snap.projects[2].name, "beta");
        assert_eq!(snap.projects[2].status_label, "open_tasks");
        assert_eq!(snap.projects[2].open_tasks, 1);
    }
}
