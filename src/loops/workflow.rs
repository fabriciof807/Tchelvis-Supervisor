use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::str::FromStr;

pub const DEFAULT_MAX_CYCLES: u32 = 3;
pub const DEFAULT_STAGE_TIMEOUT_SECS: u64 = 1800;
pub const DEFAULT_STALL_SECS: u64 = 600;
pub const DEFAULT_MAX_DURATION_SECS: u64 = 7200;
const LIMIT_MAX_CYCLES: u64 = 20;
const LIMIT_SECS: u64 = 7 * 24 * 3600;

pub const PRESET_WORKFLOWS: &[(&str, &str)] = &[
    (
        "worker-reviewer-critic",
        include_str!("../../assets/loops/workflows/worker-reviewer-critic.toml"),
    ),
    (
        "designer-worker-reviewer",
        include_str!("../../assets/loops/workflows/designer-worker-reviewer.toml"),
    ),
    (
        "planner-worker-tester-critic",
        include_str!("../../assets/loops/workflows/planner-worker-tester-critic.toml"),
    ),
    (
        "fast-worker-reviewer",
        include_str!("../../assets/loops/workflows/fast-worker-reviewer.toml"),
    ),
];

pub const DEFAULT_ROLES: &[(&str, &str)] = &[
    (
        "designer",
        include_str!("../../assets/loops/roles/designer.md"),
    ),
    (
        "planner",
        include_str!("../../assets/loops/roles/planner.md"),
    ),
    ("worker", include_str!("../../assets/loops/roles/worker.md")),
    ("tester", include_str!("../../assets/loops/roles/tester.md")),
    (
        "reviewer",
        include_str!("../../assets/loops/roles/reviewer.md"),
    ),
    ("critic", include_str!("../../assets/loops/roles/critic.md")),
];

const WORKFLOW_KEYS: &[&str] = &[
    "name",
    "description",
    "max_cycles",
    "stage_timeout_secs",
    "stall_secs",
    "max_duration_secs",
    "stages",
];

const STAGE_KEYS: &[&str] = &[
    "id",
    "role",
    "provider",
    "model",
    "reads",
    "writes",
    "edits",
    "next",
    "verdict_gate",
    "on_pass",
    "on_retry",
];

const RESERVED_READ_AND_WRITE: &[&str] = &["state.json", "report.md", "report.html", "logs"];
const BUILTIN_READS: &[&str] = &["input.md", "current.diff"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Agy,
    Opencode,
}

impl Provider {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Agy => "agy",
            Self::Opencode => "opencode",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "agy" => Ok(Self::Agy),
            "opencode" => Ok(Self::Opencode),
            other => Err(format!("provider desconhecido '{other}'")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleDefault {
    pub provider: Provider,
    #[serde(default)]
    pub model: Option<String>,
}

pub type RoleDefaults = BTreeMap<String, RoleDefault>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub id: String,
    pub role: String,
    pub provider: Provider,
    pub model: Option<String>,
    pub reads: Vec<String>,
    pub writes: Option<String>,
    pub edits: bool,
    pub next: Option<String>,
    pub verdict_gate: bool,
    pub on_pass: Option<String>,
    pub on_retry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    pub max_cycles: u32,
    pub stage_timeout_secs: u64,
    pub stall_secs: u64,
    pub max_duration_secs: u64,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedStage {
    pub id: String,
    pub role: String,
    pub provider: Option<Provider>,
    pub model: Option<String>,
    pub reads: Vec<String>,
    pub writes: Option<String>,
    pub edits: bool,
    pub next: Option<String>,
    pub verdict_gate: bool,
    pub on_pass: Option<String>,
    pub on_retry: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedWorkflow {
    pub name: String,
    pub description: String,
    pub max_cycles: u32,
    pub stage_timeout_secs: u64,
    pub stall_secs: u64,
    pub max_duration_secs: u64,
    pub stages: Vec<UnresolvedStage>,
}

pub(crate) fn line_at(raw: &str, offset: usize) -> usize {
    let end = offset.min(raw.len());
    raw.as_bytes()[..end]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// Formats a TOML parse error using only line number and reason, never raw source text.
pub(crate) fn toml_error_reason(raw: &str, err: &toml::de::Error) -> String {
    let line = err
        .span()
        .map(|span| format!("linha {}: ", line_at(raw, span.start)))
        .unwrap_or_default();
    format!("{line}{}", err.message())
}

/// A model name is an argument of a CLI: it must not look like an option nor hide characters.
/// Inner spaces are valid (`GPT-OSS 120B (Medium)`).
pub(crate) fn validate_model_name(model: &str) -> Result<(), String> {
    if model.trim().is_empty() {
        return Err("'model' não pode ser vazio".to_string());
    }
    if model.len() > 200
        || model.starts_with('-')
        || model.trim() != model
        || model.chars().any(char::is_control)
    {
        return Err(format!("'model' inválido: {model:?}"));
    }
    Ok(())
}

pub(crate) fn is_valid_stage_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id != "done"
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `[a-z0-9-]`, not starting with `-`: safe as a path, a branch component and an argument.
pub(crate) fn is_valid_slug(name: &str, max_len: usize) -> bool {
    !name.is_empty()
        && name.len() <= max_len
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn is_valid_role_name(role: &str) -> bool {
    is_valid_slug(role, 32)
}

/// `c<cycle>-…` is how the engine archives each cycle's artifacts.
fn is_cycle_archive_name(name: &str) -> bool {
    name.strip_prefix('c').is_some_and(|rest| {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        digits > 0 && rest[digits..].starts_with('-')
    })
}

fn check_artifact_name(
    name: &str,
    is_write: bool,
    stage_label: &str,
    errors: &mut Vec<String>,
) -> bool {
    let plain = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
    if !plain || is_cycle_archive_name(name) {
        errors.push(format!(
            "etapa '{stage_label}': nome de artefato inválido {name:?} (use letras, números, '.', '-' e '_', sem '..', '.' ou '-' no início, nem o prefixo 'c<n>-')"
        ));
        return false;
    }
    if RESERVED_READ_AND_WRITE.contains(&name) || (is_write && BUILTIN_READS.contains(&name)) {
        errors.push(format!(
            "etapa '{stage_label}': nome de artefato reservado '{name}'"
        ));
        return false;
    }
    true
}

fn read_positive_u64(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    default: u64,
    max: u64,
    errors: &mut Vec<String>,
) -> u64 {
    match table.get(key) {
        None => default,
        Some(toml::Value::Integer(v)) => {
            if *v <= 0 {
                errors.push(format!(
                    "limite '{key}' deve ser maior que zero (recebido {v})"
                ));
                0
            } else if *v as u64 > max {
                errors.push(format!(
                    "limite '{key}' deve ser no máximo {max} (recebido {v})"
                ));
                0
            } else {
                *v as u64
            }
        }
        Some(_) => {
            errors.push(format!("campo '{key}' deve ser um número inteiro positivo"));
            0
        }
    }
}

pub fn parse_workflow_unresolved(raw: &str) -> Result<UnresolvedWorkflow, Vec<String>> {
    parse_workflow_inner(raw, None)
}

pub fn parse_workflow(raw: &str, defaults: &RoleDefaults) -> Result<Workflow, Vec<String>> {
    let unresolved = parse_workflow_inner(raw, Some(defaults))?;
    Ok(Workflow {
        name: unresolved.name,
        description: unresolved.description,
        max_cycles: unresolved.max_cycles,
        stage_timeout_secs: unresolved.stage_timeout_secs,
        stall_secs: unresolved.stall_secs,
        max_duration_secs: unresolved.max_duration_secs,
        stages: unresolved
            .stages
            .into_iter()
            .map(|s| Stage {
                id: s.id,
                role: s.role,
                provider: s.provider.expect("parse_workflow_inner resolved provider"),
                model: s.model,
                reads: s.reads,
                writes: s.writes,
                edits: s.edits,
                next: s.next,
                verdict_gate: s.verdict_gate,
                on_pass: s.on_pass,
                on_retry: s.on_retry,
            })
            .collect(),
    })
}

fn parse_workflow_inner(
    raw: &str,
    defaults: Option<&RoleDefaults>,
) -> Result<UnresolvedWorkflow, Vec<String>> {
    let root: toml::Value = match toml::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return Err(vec![format!(
                "TOML inválido no workflow ({})",
                toml_error_reason(raw, &e)
            )])
        }
    };

    let Some(table) = root.as_table() else {
        return Err(vec!["workflow TOML deve ser uma tabela".to_string()]);
    };

    let mut errors = Vec::new();

    for key in table.keys() {
        if !WORKFLOW_KEYS.contains(&key.as_str()) {
            errors.push(format!("chave desconhecida no workflow: '{key}'"));
        }
    }

    let name = match table.get("name") {
        Some(toml::Value::String(s)) if is_valid_slug(s, 48) => s.clone(),
        Some(toml::Value::String(s)) => {
            errors.push(format!(
                "campo 'name' do workflow inválido: {s:?} (use [a-z0-9-], até 48 caracteres)"
            ));
            String::new()
        }
        Some(_) => {
            errors.push("campo 'name' do workflow deve ser uma string".to_string());
            String::new()
        }
        None => {
            errors.push("campo obrigatório 'name' ausente no workflow".to_string());
            String::new()
        }
    };

    let description = match table.get("description") {
        None => String::new(),
        Some(toml::Value::String(s)) => s.clone(),
        Some(_) => {
            errors.push("campo 'description' do workflow deve ser uma string".to_string());
            String::new()
        }
    };

    let max_cycles_u64 = read_positive_u64(
        table,
        "max_cycles",
        u64::from(DEFAULT_MAX_CYCLES),
        LIMIT_MAX_CYCLES,
        &mut errors,
    );
    let max_cycles = u32::try_from(max_cycles_u64).expect("bounded by LIMIT_MAX_CYCLES");
    let stage_timeout_secs = read_positive_u64(
        table,
        "stage_timeout_secs",
        DEFAULT_STAGE_TIMEOUT_SECS,
        LIMIT_SECS,
        &mut errors,
    );
    let stall_secs = read_positive_u64(
        table,
        "stall_secs",
        DEFAULT_STALL_SECS,
        LIMIT_SECS,
        &mut errors,
    );
    let max_duration_secs = read_positive_u64(
        table,
        "max_duration_secs",
        DEFAULT_MAX_DURATION_SECS,
        LIMIT_SECS,
        &mut errors,
    );

    let stage_tables: Vec<&toml::map::Map<String, toml::Value>> = match table.get("stages") {
        None => {
            errors.push("workflow deve definir ao menos uma etapa em [[stages]]".to_string());
            Vec::new()
        }
        Some(toml::Value::Array(arr)) => {
            if arr.is_empty() {
                errors.push("workflow deve definir ao menos uma etapa em [[stages]]".to_string());
            }
            let mut out = Vec::with_capacity(arr.len());
            for (idx, item) in arr.iter().enumerate() {
                if let Some(st) = item.as_table() {
                    out.push(st);
                } else {
                    errors.push(format!("stages[{idx}] deve ser uma tabela"));
                }
            }
            out
        }
        Some(_) => {
            errors.push("campo 'stages' deve ser um array de tabelas".to_string());
            Vec::new()
        }
    };

    struct ParsedStageMeta {
        stage: UnresolvedStage,
        valid_id: bool,
        valid_reads: Vec<String>,
    }

    let mut seen_ids = BTreeSet::new();
    let mut parsed_stages: Vec<ParsedStageMeta> = Vec::with_capacity(stage_tables.len());

    for (idx, st) in stage_tables.iter().enumerate() {
        let mut id_valid = true;
        let raw_id = match st.get("id") {
            Some(toml::Value::String(s)) => s.clone(),
            Some(_) => {
                id_valid = false;
                errors.push(format!("stages[{idx}]: campo 'id' deve ser uma string"));
                format!("stages[{idx}]")
            }
            None => {
                id_valid = false;
                errors.push(format!("stages[{idx}]: campo obrigatório 'id' ausente"));
                format!("stages[{idx}]")
            }
        };

        let label = raw_id.clone();

        for key in st.keys() {
            if !STAGE_KEYS.contains(&key.as_str()) {
                errors.push(format!("etapa '{label}': chave desconhecida '{key}'"));
            }
        }

        if id_valid && !is_valid_stage_id(&raw_id) {
            id_valid = false;
            errors.push(format!(
                "etapa '{raw_id}': id inválido (esperado [a-z0-9-]{{1,32}} e diferente de 'done')"
            ));
        }

        if id_valid && !seen_ids.insert(raw_id.clone()) {
            id_valid = false;
            errors.push(format!("etapa '{raw_id}': id duplicado"));
        }

        let role = match st.get("role") {
            Some(toml::Value::String(s)) => {
                if !is_valid_role_name(s) {
                    errors.push(format!("etapa '{label}': role inválido '{s}'"));
                }
                s.clone()
            }
            Some(_) => {
                errors.push(format!("etapa '{label}': campo 'role' deve ser uma string"));
                String::new()
            }
            None => {
                errors.push(format!("etapa '{label}': campo obrigatório 'role' ausente"));
                String::new()
            }
        };

        let mut model_invalid_or_missing_provider = false;
        let stage_model = match st.get("model") {
            None => None,
            Some(toml::Value::String(s)) => {
                if let Err(e) = validate_model_name(s) {
                    errors.push(format!("etapa '{label}': {e}"));
                    None
                } else {
                    Some(s.clone())
                }
            }
            Some(_) => {
                errors.push(format!(
                    "etapa '{label}': campo 'model' deve ser uma string"
                ));
                model_invalid_or_missing_provider = true;
                None
            }
        };

        let (provider, model) = match st.get("provider") {
            Some(toml::Value::String(s)) => match Provider::from_str(s) {
                Ok(p) => (Some(p), stage_model),
                Err(msg) => {
                    errors.push(format!("etapa '{label}': {msg}"));
                    (Some(Provider::Claude), stage_model)
                }
            },
            Some(_) => {
                errors.push(format!(
                    "etapa '{label}': campo 'provider' deve ser uma string"
                ));
                (Some(Provider::Claude), stage_model)
            }
            None => {
                if stage_model.is_some() && !model_invalid_or_missing_provider {
                    errors.push(format!("etapa '{label}': 'model' informado sem 'provider'"));
                    (Some(Provider::Claude), stage_model)
                } else if let Some(defs) = defaults {
                    if let Some(def) = defs.get(&role) {
                        (Some(def.provider), def.model.clone())
                    } else {
                        errors.push(format!(
                            "etapa '{label}': provider ausente e sem padrão em [defaults.{role}] para o role '{role}'"
                        ));
                        (Some(Provider::Claude), None)
                    }
                } else {
                    (None, None)
                }
            }
        };

        let mut reads = Vec::new();
        let mut valid_reads = Vec::new();
        match st.get("reads") {
            None => {}
            Some(toml::Value::Array(arr)) => {
                for item in arr {
                    match item {
                        toml::Value::String(s) => {
                            if check_artifact_name(s, false, &label, &mut errors) {
                                valid_reads.push(s.clone());
                            }
                            reads.push(s.clone());
                        }
                        _ => errors.push(format!(
                            "etapa '{label}': itens de 'reads' devem ser strings"
                        )),
                    }
                }
            }
            Some(_) => errors.push(format!("etapa '{label}': campo 'reads' deve ser um array")),
        }

        let writes = match st.get("writes") {
            None => None,
            Some(toml::Value::String(s)) => {
                check_artifact_name(s, true, &label, &mut errors);
                Some(s.clone())
            }
            Some(_) => {
                errors.push(format!(
                    "etapa '{label}': campo 'writes' deve ser uma string"
                ));
                None
            }
        };

        let edits = match st.get("edits") {
            None => false,
            Some(toml::Value::Boolean(b)) => *b,
            Some(_) => {
                errors.push(format!("etapa '{label}': campo 'edits' deve ser booleano"));
                false
            }
        };

        let verdict_gate = match st.get("verdict_gate") {
            None => false,
            Some(toml::Value::Boolean(b)) => *b,
            Some(_) => {
                errors.push(format!(
                    "etapa '{label}': campo 'verdict_gate' deve ser booleano"
                ));
                false
            }
        };

        let read_opt_str = |field: &str, errs: &mut Vec<String>| -> Option<String> {
            match st.get(field) {
                None => None,
                Some(toml::Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    errs.push(format!(
                        "etapa '{label}': campo '{field}' deve ser uma string"
                    ));
                    None
                }
            }
        };

        let next = read_opt_str("next", &mut errors);
        let on_pass = read_opt_str("on_pass", &mut errors);
        let on_retry = read_opt_str("on_retry", &mut errors);

        if verdict_gate {
            if writes.is_none() {
                errors.push(format!(
                    "etapa '{label}': etapa com verdict_gate exige 'writes'"
                ));
            }
            if next.is_some() {
                errors.push(format!(
                    "etapa '{label}': etapa com verdict_gate não pode definir 'next'"
                ));
            }
            if on_pass.is_none() {
                errors.push(format!(
                    "etapa '{label}': etapa com verdict_gate exige 'on_pass'"
                ));
            }
            if on_retry.is_none() {
                errors.push(format!(
                    "etapa '{label}': etapa com verdict_gate exige 'on_retry'"
                ));
            }
        } else {
            if on_pass.is_some() {
                errors.push(format!(
                    "etapa '{label}': etapa sem verdict_gate não pode definir 'on_pass'"
                ));
            }
            if on_retry.is_some() {
                errors.push(format!(
                    "etapa '{label}': etapa sem verdict_gate não pode definir 'on_retry'"
                ));
            }
            if next.is_none() {
                errors.push(format!(
                    "etapa '{label}': etapa sem verdict_gate exige 'next'"
                ));
            }
        }

        parsed_stages.push(ParsedStageMeta {
            stage: UnresolvedStage {
                id: raw_id,
                role,
                provider,
                model,
                reads,
                writes,
                edits,
                next,
                verdict_gate,
                on_pass,
                on_retry,
            },
            valid_id: id_valid,
            valid_reads,
        });
    }

    let produced_writes: BTreeSet<&str> = parsed_stages
        .iter()
        .filter_map(|m| m.stage.writes.as_deref())
        .collect();

    for meta in &parsed_stages {
        for r in &meta.valid_reads {
            if !BUILTIN_READS.contains(&r.as_str()) && !produced_writes.contains(r.as_str()) {
                errors.push(format!(
                    "etapa '{}': 'reads' referencia '{r}', que nenhuma etapa produz em 'writes'",
                    meta.stage.id
                ));
            }
        }
    }

    let valid_stage_ids: BTreeSet<&str> = parsed_stages
        .iter()
        .filter(|m| m.valid_id)
        .map(|m| m.stage.id.as_str())
        .collect();

    let mut stage_has_unknown_target: BTreeSet<&str> = BTreeSet::new();
    for meta in &parsed_stages {
        let st = &meta.stage;
        for (field, target) in [
            ("next", st.next.as_deref()),
            ("on_pass", st.on_pass.as_deref()),
            ("on_retry", st.on_retry.as_deref()),
        ] {
            if let Some(t) = target {
                if t == "done" && field != "on_pass" {
                    errors.push(format!(
                        "etapa '{}': '{field}' não pode ir para 'done'; só 'on_pass' de uma etapa com verdict_gate encerra o loop",
                        st.id
                    ));
                } else if t != "done" && !valid_stage_ids.contains(t) {
                    stage_has_unknown_target.insert(st.id.as_str());
                    errors.push(format!(
                        "etapa '{}': destino desconhecido '{t}' em '{field}'",
                        st.id
                    ));
                }
            }
        }
    }

    if !parsed_stages.is_empty() && !valid_stage_ids.is_empty() {
        let mut stage_by_id: BTreeMap<&str, &UnresolvedStage> = BTreeMap::new();
        for meta in &parsed_stages {
            if meta.valid_id {
                stage_by_id.insert(meta.stage.id.as_str(), &meta.stage);
            }
        }

        // Reachability from entry stage
        let mut reachable: BTreeSet<&str> = BTreeSet::new();
        if let Some(first) = parsed_stages.first().filter(|m| m.valid_id) {
            let mut queue = VecDeque::new();
            reachable.insert(first.stage.id.as_str());
            queue.push_back(first.stage.id.as_str());
            while let Some(cur) = queue.pop_front() {
                if let Some(st) = stage_by_id.get(cur) {
                    for target in [
                        st.next.as_deref(),
                        st.on_pass.as_deref(),
                        st.on_retry.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if target != "done"
                            && stage_by_id.contains_key(target)
                            && reachable.insert(target)
                        {
                            queue.push_back(target);
                        }
                    }
                }
            }
        }

        for meta in &parsed_stages {
            if meta.valid_id && !reachable.contains(meta.stage.id.as_str()) {
                errors.push(format!(
                    "etapa '{}' é inalcançável a partir da etapa inicial",
                    meta.stage.id
                ));
            }
        }

        // Backwards reachability from "done"
        let mut rev_edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (&id, &st) in &stage_by_id {
            for target in [
                st.next.as_deref(),
                st.on_pass.as_deref(),
                st.on_retry.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                rev_edges.entry(target).or_default().push(id);
            }
        }

        let mut can_reach_done: BTreeSet<&str> = BTreeSet::new();
        let mut q_done = VecDeque::new();
        if let Some(preds) = rev_edges.get("done") {
            for &p in preds {
                if can_reach_done.insert(p) {
                    q_done.push_back(p);
                }
            }
        }
        while let Some(cur) = q_done.pop_front() {
            if let Some(preds) = rev_edges.get(cur) {
                for &p in preds {
                    if can_reach_done.insert(p) {
                        q_done.push_back(p);
                    }
                }
            }
        }

        for meta in &parsed_stages {
            if meta.valid_id
                && reachable.contains(meta.stage.id.as_str())
                && !stage_has_unknown_target.contains(meta.stage.id.as_str())
                && !can_reach_done.contains(meta.stage.id.as_str())
            {
                errors.push(format!(
                    "etapa '{}': nenhum caminho até 'done'",
                    meta.stage.id
                ));
            }
        }

        // Only RETRY counts as a cycle, so the graph without `on_retry` edges must be acyclic.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Color {
            Visiting,
            Done,
        }
        let mut color: BTreeMap<&str, Color> = BTreeMap::new();
        for meta in &parsed_stages {
            if !meta.valid_id || color.contains_key(meta.stage.id.as_str()) {
                continue;
            }
            let mut path: Vec<&str> = Vec::new();
            let mut cur_opt = Some(meta.stage.id.as_str());
            while let Some(cur) = cur_opt {
                if let Some(c) = color.get(cur) {
                    if *c == Color::Visiting {
                        if let Some(pos) = path.iter().position(|&id| id == cur) {
                            let mut cycle: Vec<&str> = path[pos..].to_vec();
                            cycle.push(cur);
                            errors.push(format!(
                                "ciclo sem verdict_gate em que RETRY conte, envolvendo: {}",
                                cycle.join(" -> ")
                            ));
                        }
                    }
                    break;
                }
                color.insert(cur, Color::Visiting);
                path.push(cur);
                cur_opt = stage_by_id
                    .get(cur)
                    .and_then(|st| {
                        if st.verdict_gate {
                            st.on_pass.as_deref()
                        } else {
                            st.next.as_deref()
                        }
                    })
                    .filter(|t| stage_by_id.contains_key(t));
            }
            for node in path {
                color.insert(node, Color::Done);
            }
        }
    }

    if errors.is_empty() {
        Ok(UnresolvedWorkflow {
            name,
            description,
            max_cycles,
            stage_timeout_secs,
            stall_secs,
            max_duration_secs,
            stages: parsed_stages.into_iter().map(|m| m.stage).collect(),
        })
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_defaults() -> RoleDefaults {
        let mut map = RoleDefaults::new();
        for role in [
            "designer", "planner", "worker", "tester", "reviewer", "critic",
        ] {
            map.insert(
                role.to_string(),
                RoleDefault {
                    provider: Provider::Claude,
                    model: Some("test-model".to_string()),
                },
            );
        }
        map
    }

    #[test]
    fn valid_example_parses() {
        let raw = r#"
name = "worker-reviewer-critic"
description = "Implementa, revisa e audita antes de aprovar"
max_cycles = 3
stage_timeout_secs = 1800
stall_secs = 600
max_duration_secs = 7200

[[stages]]
id = "implement"
role = "worker"
provider = "opencode"
model = "zai/glm-5.3-flash"
reads = ["input.md", "review.md", "critic.md"]
edits = true
next = "review"

[[stages]]
id = "review"
role = "reviewer"
provider = "agy"
model = "gemini-4-hydrogen"
reads = ["input.md", "current.diff"]
writes = "review.md"
next = "audit"

[[stages]]
id = "audit"
role = "critic"
provider = "claude"
reads = ["input.md", "current.diff", "review.md"]
writes = "critic.md"
verdict_gate = true
on_pass = "done"
on_retry = "implement"
"#;
        let wf = parse_workflow(raw, &RoleDefaults::new()).expect("spec example must parse");
        assert_eq!(wf.name, "worker-reviewer-critic");
        assert_eq!(
            wf.description,
            "Implementa, revisa e audita antes de aprovar"
        );
        assert_eq!(wf.max_cycles, 3);
        assert_eq!(wf.stage_timeout_secs, 1800);
        assert_eq!(wf.stall_secs, 600);
        assert_eq!(wf.max_duration_secs, 7200);
        assert_eq!(wf.stages.len(), 3);

        assert_eq!(wf.stages[0].id, "implement");
        assert_eq!(wf.stages[0].provider, Provider::Opencode);
        assert_eq!(wf.stages[0].model.as_deref(), Some("zai/glm-5.3-flash"));
        assert!(wf.stages[0].edits);
        assert_eq!(wf.stages[0].next.as_deref(), Some("review"));

        assert_eq!(wf.stages[1].id, "review");
        assert_eq!(wf.stages[1].provider, Provider::Agy);
        assert_eq!(wf.stages[1].writes.as_deref(), Some("review.md"));
        assert!(!wf.stages[1].edits);

        assert_eq!(wf.stages[2].id, "audit");
        assert_eq!(wf.stages[2].provider, Provider::Claude);
        assert_eq!(wf.stages[2].model, None);
        assert!(wf.stages[2].verdict_gate);
        assert_eq!(wf.stages[2].on_pass.as_deref(), Some("done"));
        assert_eq!(wf.stages[2].on_retry.as_deref(), Some("implement"));
    }

    #[test]
    fn each_rule_is_reported() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "duplicate id",
                "dup-stage",
                r#"
name = "w"
[[stages]]
id = "dup-stage"
role = "worker"
provider = "claude"
next = "gate"
[[stages]]
id = "dup-stage"
role = "worker"
provider = "claude"
next = "gate"
[[stages]]
id = "gate"
role = "critic"
provider = "claude"
writes = "critic.md"
verdict_gate = true
on_pass = "done"
on_retry = "dup-stage"
"#,
            ),
            (
                "invalid id characters",
                "Bad_Id",
                r#"
name = "w"
[[stages]]
id = "Bad_Id"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
            (
                "id over 32 chars",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                r#"
name = "w"
[[stages]]
id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
            (
                "unknown next",
                "s1",
                r#"
name = "w"
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
next = "ghost"
"#,
            ),
            (
                "unknown on_pass",
                "g1",
                r#"
name = "w"
[[stages]]
id = "g1"
role = "critic"
provider = "claude"
writes = "c.md"
verdict_gate = true
on_pass = "missing-pass"
on_retry = "g1"
"#,
            ),
            (
                "unknown on_retry",
                "g2",
                r#"
name = "w"
[[stages]]
id = "g2"
role = "critic"
provider = "claude"
writes = "c.md"
verdict_gate = true
on_pass = "done"
on_retry = "missing-retry"
"#,
            ),
            (
                "unreachable stage",
                "orphan",
                r#"
name = "w"
[[stages]]
id = "start"
role = "worker"
provider = "claude"
next = "done"
[[stages]]
id = "orphan"
role = "reviewer"
provider = "claude"
next = "done"
"#,
            ),
            (
                "no path to done",
                "trapped",
                r#"
name = "w"
[[stages]]
id = "trapped"
role = "worker"
provider = "claude"
next = "gate"
[[stages]]
id = "gate"
role = "critic"
provider = "claude"
writes = "c.md"
verdict_gate = true
on_pass = "trapped"
on_retry = "trapped"
"#,
            ),
            (
                "cycle without a gate",
                "loop-a",
                r#"
name = "w"
[[stages]]
id = "loop-a"
role = "worker"
provider = "claude"
next = "loop-b"
[[stages]]
id = "loop-b"
role = "reviewer"
provider = "claude"
next = "loop-a"
"#,
            ),
            (
                "gate without writes",
                "gateless-write",
                r#"
name = "w"
[[stages]]
id = "gateless-write"
role = "critic"
provider = "claude"
verdict_gate = true
on_pass = "done"
on_retry = "gateless-write"
"#,
            ),
            (
                "gate with next",
                "gate-with-next",
                r#"
name = "w"
[[stages]]
id = "gate-with-next"
role = "critic"
provider = "claude"
writes = "c.md"
verdict_gate = true
next = "done"
on_pass = "done"
on_retry = "gate-with-next"
"#,
            ),
            (
                "non-gate with on_pass",
                "nongate-pass",
                r#"
name = "w"
[[stages]]
id = "nongate-pass"
role = "worker"
provider = "claude"
next = "done"
on_pass = "done"
"#,
            ),
            (
                "non-gate with on_retry",
                "nongate-retry",
                r#"
name = "w"
[[stages]]
id = "nongate-retry"
role = "worker"
provider = "claude"
next = "done"
on_retry = "nongate-retry"
"#,
            ),
            (
                "non-gate without next",
                "nongate-nonext",
                r#"
name = "w"
[[stages]]
id = "nongate-nonext"
role = "worker"
provider = "claude"
"#,
            ),
            (
                "artifact with slash",
                "art-slash",
                r#"
name = "w"
[[stages]]
id = "art-slash"
role = "reviewer"
provider = "claude"
writes = "sub/review.md"
next = "done"
"#,
            ),
            (
                "artifact with dotdot",
                "art-dotdot",
                r#"
name = "w"
[[stages]]
id = "art-dotdot"
role = "reviewer"
provider = "claude"
writes = "..review.md"
next = "done"
"#,
            ),
            (
                "artifact with leading dot",
                "art-dot",
                r#"
name = "w"
[[stages]]
id = "art-dot"
role = "reviewer"
provider = "claude"
writes = ".hidden.md"
next = "done"
"#,
            ),
            (
                "reserved artifact input.md in writes",
                "res-input",
                r#"
name = "w"
[[stages]]
id = "res-input"
role = "worker"
provider = "claude"
writes = "input.md"
next = "done"
"#,
            ),
            (
                "reserved artifact current.diff in writes",
                "res-diff",
                r#"
name = "w"
[[stages]]
id = "res-diff"
role = "worker"
provider = "claude"
writes = "current.diff"
next = "done"
"#,
            ),
            (
                "reserved artifact state.json",
                "res-state",
                r#"
name = "w"
[[stages]]
id = "res-state"
role = "worker"
provider = "claude"
reads = ["state.json"]
next = "done"
"#,
            ),
            (
                "reserved artifact report.md",
                "res-report",
                r#"
name = "w"
[[stages]]
id = "res-report"
role = "worker"
provider = "claude"
writes = "report.md"
next = "done"
"#,
            ),
            (
                "reads nobody produces",
                "unproduced-read",
                r#"
name = "w"
[[stages]]
id = "unproduced-read"
role = "worker"
provider = "claude"
reads = ["ghost.md"]
next = "done"
"#,
            ),
            (
                "unknown provider",
                "bad-prov",
                r#"
name = "w"
[[stages]]
id = "bad-prov"
role = "worker"
provider = "gpt4"
next = "done"
"#,
            ),
            (
                "provider missing with no default",
                "no-def",
                r#"
name = "w"
[[stages]]
id = "no-def"
role = "wizard"
next = "done"
"#,
            ),
            (
                "unknown top-level key",
                "mystery_top",
                r#"
name = "w"
mystery_top = true
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
            (
                "unknown stage key",
                "mystery_stage",
                r#"
name = "w"
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
mystery_stage = 42
next = "done"
"#,
            ),
            (
                "zero max_cycles",
                "max_cycles",
                r#"
name = "w"
max_cycles = 0
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
            (
                "zero stage_timeout_secs",
                "stage_timeout_secs",
                r#"
name = "w"
stage_timeout_secs = 0
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
            (
                "zero stall_secs",
                "stall_secs",
                r#"
name = "w"
stall_secs = 0
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
            (
                "zero max_duration_secs",
                "max_duration_secs",
                r#"
name = "w"
max_duration_secs = 0
[[stages]]
id = "s1"
role = "worker"
provider = "claude"
next = "done"
"#,
            ),
        ];

        let defaults = RoleDefaults::new();
        for (rule, must_mention, toml_src) in cases {
            let errs =
                parse_workflow(toml_src, &defaults).expect_err(&format!("rule '{rule}' must fail"));
            let joined = errs.join("\n");
            assert!(
                joined.contains(must_mention),
                "rule '{rule}': expected error mentioning '{must_mention}', got:\n{joined}"
            );
        }
    }

    #[test]
    fn two_problems_are_both_reported() {
        let raw = r#"
name = "broken"
max_cycles = 0

[[stages]]
id = "first"
role = "worker"
provider = "unknown-cli"
reads = ["missing.md"]
next = "done"
"#;
        let errs = parse_workflow(raw, &RoleDefaults::new()).expect_err("must fail");
        let joined = errs.join("\n");
        assert!(errs.len() >= 2, "expected >=2 errors, got {errs:?}");
        assert!(
            joined.contains("max_cycles"),
            "missing max_cycles: {joined}"
        );
        assert!(
            joined.contains("unknown-cli"),
            "missing unknown-cli: {joined}"
        );
        assert!(
            joined.contains("missing.md"),
            "missing missing.md: {joined}"
        );
    }

    #[test]
    fn presets_validate_with_defaults() {
        let defaults = full_defaults();
        assert_eq!(PRESET_WORKFLOWS.len(), 4);
        assert_eq!(DEFAULT_ROLES.len(), 6);
        for (name, raw) in PRESET_WORKFLOWS {
            let wf = parse_workflow(raw, &defaults)
                .unwrap_or_else(|e| panic!("preset '{name}' failed validation: {e:?}"));
            assert_eq!(wf.name, *name);
            for st in &wf.stages {
                assert!(
                    DEFAULT_ROLES.iter().any(|(r, _)| *r == st.role),
                    "preset '{name}' references unknown default role '{}'",
                    st.role
                );
            }
        }
        for (role, md) in DEFAULT_ROLES {
            assert!(!md.trim().is_empty(), "role '{role}' markdown is empty");
        }
    }

    #[test]
    fn presets_without_defaults_name_the_missing_role() {
        let empty = RoleDefaults::new();
        for (name, raw) in PRESET_WORKFLOWS {
            let errs = parse_workflow(raw, &empty)
                .expect_err(&format!("preset '{name}' without defaults must fail"));
            let joined = errs.join("\n");
            assert!(
                joined.contains("worker"),
                "preset '{name}' error should name missing role 'worker', got:\n{joined}"
            );
        }

        let mut partial = full_defaults();
        partial.remove("tester");
        let planner_raw = PRESET_WORKFLOWS
            .iter()
            .find(|(n, _)| *n == "planner-worker-tester-critic")
            .unwrap()
            .1;
        let errs = parse_workflow(planner_raw, &partial).expect_err("missing tester must fail");
        assert!(
            errs.join("\n").contains("tester"),
            "expected 'tester' in error: {errs:?}"
        );
    }

    fn two_stage(name: &str, gate: &str) -> String {
        format!(
            r#"name = "{name}"
[[stages]]
id = "a"
role = "worker"
provider = "claude"
edits = true
next = "b"
[[stages]]
id = "b"
role = "critic"
provider = "claude"
writes = "critic.md"
verdict_gate = true
{gate}
"#
        )
    }

    const GATE_OK: &str = "on_pass = \"done\"\non_retry = \"a\"";

    fn errors(raw: &str) -> String {
        parse_workflow(raw, &RoleDefaults::new())
            .expect_err("must be rejected")
            .join("\n")
    }

    #[test]
    fn workflow_name_is_safe_for_branches_and_paths() {
        assert!(parse_workflow(&two_stage("ok-1", GATE_OK), &RoleDefaults::new()).is_ok());
        for name in [
            "../x", "a/b", "a b", "-x", "x..y", "x.lock", "A", "á", "a\\nb",
        ] {
            assert!(errors(&two_stage(name, GATE_OK)).contains("name"), "{name}");
        }
        assert!(errors(&two_stage(&"x".repeat(49), GATE_OK)).contains("name"));
    }

    #[test]
    fn only_a_gate_approval_may_finish_and_retry_never_does() {
        let retry_done = two_stage("w", "on_pass = \"done\"\non_retry = \"done\"");
        assert!(errors(&retry_done).contains("on_retry"));
        let no_gate = "name = \"w\"\n[[stages]]\nid = \"a\"\nrole = \"worker\"\nprovider = \"claude\"\nnext = \"done\"\n";
        assert!(errors(no_gate).contains("verdict_gate"));
    }

    #[test]
    fn a_cycle_that_only_follows_approvals_is_rejected() {
        // PASS would loop forever: only RETRY counts as a cycle.
        let raw = r#"name = "w"
[[stages]]
id = "a"
role = "critic"
provider = "claude"
writes = "a.md"
verdict_gate = true
on_pass = "b"
on_retry = "c"
[[stages]]
id = "b"
role = "worker"
provider = "claude"
next = "a"
[[stages]]
id = "c"
role = "critic"
provider = "claude"
writes = "c.md"
verdict_gate = true
on_pass = "done"
on_retry = "a"
"#;
        assert!(errors(raw).contains("ciclo"));
    }

    #[test]
    fn artifact_and_role_names_are_plain() {
        for bad in [
            "a\\nb.md",
            "a b.md",
            "logs",
            "c1-review.md",
            "-x.md",
            "é.md",
        ] {
            let raw = two_stage("w", GATE_OK).replace("critic.md", bad);
            assert!(errors(&raw).contains("artefato"), "{bad}");
        }
        for bad in ["a b", "a\\nb", "-x", "á"] {
            let raw = two_stage("w", GATE_OK).replace("\"worker\"", &format!("\"{bad}\""));
            assert!(errors(&raw).contains("role"), "{bad}");
        }
    }

    #[test]
    fn limits_have_an_upper_bound() {
        for limit in [
            "max_cycles = 21",
            "stage_timeout_secs = 604801",
            "stall_secs = 9223372036854775807",
            "max_duration_secs = 604801",
        ] {
            let raw = two_stage("w", GATE_OK).replacen("name", &format!("{limit}\nname"), 1);
            let key = limit.split(' ').next().unwrap();
            assert!(errors(&raw).contains(key), "{limit}");
        }
    }

    #[test]
    fn model_without_provider_is_rejected_and_unresolved_keeps_missing_provider() {
        let missing_provider_for_model = r#"
name = "w"
[[stages]]
id = "a"
role = "worker"
model = "claude-sonnet-4"
edits = true
next = "b"
[[stages]]
id = "b"
role = "critic"
provider = "claude"
writes = "critic.md"
verdict_gate = true
on_pass = "done"
on_retry = "a"
"#;
        let errs = parse_workflow_unresolved(missing_provider_for_model).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("model") && e.contains("provider")),
            "expected error about model without provider, got: {errs:?}"
        );
        let errs2 = parse_workflow(missing_provider_for_model, &full_defaults()).unwrap_err();
        assert!(
            errs2
                .iter()
                .any(|e| e.contains("model") && e.contains("provider")),
            "expected parse_workflow error about model without provider, got: {errs2:?}"
        );

        let preset = PRESET_WORKFLOWS[0].1;
        let unresolved = parse_workflow_unresolved(preset).expect("preset parses unresolved");
        assert!(unresolved.stages.iter().all(|s| s.provider.is_none()));
    }
}
