use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

use super::config::{LlmBackendKind, SupervisorConfig};
use super::state::{AgentDurableState, LifecycleState, Urgency};

/// Decision emitted by the cognitive evaluation layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SupervisorDecision {
    NoOp,
    AutoReply,
    AlertOperator,
}

impl SupervisorDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoOp => "NO_OP",
            Self::AutoReply => "AUTO_REPLY",
            Self::AlertOperator => "ALERT_OPERATOR",
        }
    }
}

/// Structured output returned by Ollama for a single agent evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CognitiveEvaluation {
    pub updated_summary: String,
    pub detected_state: LifecycleState,
    pub decision: SupervisorDecision,
    pub urgency: Urgency,
    pub hc_command_payload: String,
    pub reason_for_operator: String,
}

/// Abstraction over the cognitive evaluation backend so unit tests can mock Ollama cleanly.
pub trait CognitiveBackend: Send + Sync {
    fn evaluate(
        &self,
        state: &AgentDurableState,
        recent_output: &str,
    ) -> Result<CognitiveEvaluation>;

    fn unload_model(&self) -> Result<()>;
}

/// Live connectivity and model availability status for the configured local/remote LLM backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LlmHealthStatus {
    pub backend: String,
    pub host: String,
    pub model: String,
    pub num_gpu: u32,
    pub connected: bool,
    pub model_installed: bool,
    pub model_loaded: bool,
    pub detail: String,
}

fn ollama_model_matches(candidate: &str, target: &str) -> bool {
    if candidate == target {
        return true;
    }
    if candidate.starts_with(&format!("{target}:")) {
        return true;
    }
    let cand_base = candidate.split(':').next().unwrap_or("");
    !cand_base.is_empty() && target.starts_with(cand_base) && candidate == target
}

fn extract_ollama_model_names(val: &Value) -> Vec<String> {
    val.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("model").and_then(|v| v.as_str()))
                        .map(ToString::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Probes the configured LLM backend (`Ollama`, `LlamaCpp`, `LmStudio`, or `OpenAiCompatible`)
/// with a short timeout to determine reachability, installed model presence, and RAM residency.
pub fn probe_llm_health(config: &SupervisorConfig) -> LlmHealthStatus {
    let primary_host = config.effective_primary_host();
    let num_gpu = config.effective_num_gpu();
    let mut status = LlmHealthStatus {
        backend: config.llm_backend.as_str().to_string(),
        host: primary_host.clone(),
        model: config.ollama_model.clone(),
        num_gpu,
        connected: false,
        model_installed: false,
        model_loaded: false,
        detail: String::new(),
    };

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(1200))
        .build();

    match config.llm_backend {
        LlmBackendKind::Ollama => {
            let mut hosts = vec![primary_host.clone()];
            if let Some(ref fb) = config.ollama_fallback_host {
                let trimmed = fb.trim_end_matches('/').to_string();
                if !trimmed.is_empty() && trimmed != primary_host {
                    hosts.push(trimmed);
                }
            }

            let mut active_host: Option<String> = None;
            let mut installed_names: Vec<String> = Vec::new();
            for host in &hosts {
                let url = format!("{}/api/tags", host.trim_end_matches('/'));
                if let Ok(resp) = agent.get(&url).call() {
                    if let Ok(val) = resp.into_json::<Value>() {
                        installed_names = extract_ollama_model_names(&val);
                        active_host = Some(host.clone());
                        break;
                    }
                }
            }

            if let Some(active) = active_host {
                status.host = active.clone();
                status.connected = true;
                status.model_installed = installed_names
                    .iter()
                    .any(|name| ollama_model_matches(name, &config.ollama_model));

                let ps_url = format!("{}/api/ps", active.trim_end_matches('/'));
                if let Ok(resp) = agent.get(&ps_url).call() {
                    if let Ok(val) = resp.into_json::<Value>() {
                        let loaded_names = extract_ollama_model_names(&val);
                        status.model_loaded = loaded_names
                            .iter()
                            .any(|name| ollama_model_matches(name, &config.ollama_model));
                    }
                }

                status.detail = if status.model_installed && status.model_loaded {
                    format!(
                        "Conectado ({active}) · {} carregado em RAM (num_gpu={})",
                        config.ollama_model, num_gpu
                    )
                } else if status.model_installed {
                    format!(
                        "Conectado ({active}) · {} pronto (standby 0B · num_gpu={})",
                        config.ollama_model, num_gpu
                    )
                } else {
                    format!(
                        "Ollama online ({active}), mas modelo '{}' não encontrado (rode: ollama pull {})",
                        config.ollama_model, config.ollama_model
                    )
                };
            } else {
                status.detail = format!("Offline ({primary_host}) · inicie com: ollama serve");
            }
        }
        LlmBackendKind::LlamaCpp | LlmBackendKind::LmStudio => {
            let base = primary_host.trim_end_matches('/');
            let models_url = format!("{base}/v1/models");
            let health_url = format!("{base}/health");
            let ok = agent.get(&models_url).call().is_ok() || agent.get(&health_url).call().is_ok();
            if ok {
                status.connected = true;
                status.model_installed = true;
                status.detail = format!("Conectado ({primary_host}) · {}", config.ollama_model);
            } else {
                status.detail = format!("Offline ({primary_host})");
            }
        }
        LlmBackendKind::OpenAiCompatible => {
            let has_key = config
                .api_key
                .as_ref()
                .is_some_and(|k| !k.trim().is_empty());
            status.connected = has_key;
            status.model_installed = has_key;
            status.detail = if has_key {
                format!("API configurada ({primary_host}) · {}", config.ollama_model)
            } else {
                "API Key ausente (defina COCKPIT_LLM_API_KEY)".to_string()
            };
        }
    }

    status
}

/// Embedded system prompt for the Cockpit Durable Supervisor.
pub const SYSTEM_PROMPT: &str = r#"You are the Cockpit Durable Supervisor, an autonomous triage controller monitoring software engineering agents via Herdr Core (hc).
Your job is to analyze an agent's durable state and its last 25 lines of terminal output, then output a strict JSON decision.

RULES FOR DECISION ("NO_OP", "AUTO_REPLY", "ALERT_OPERATOR"):
1. Choose "NO_OP" if the agent is still compiling, downloading, running long tests normally, or requires no action.
2. Choose "AUTO_REPLY" ONLY for safe, reversible, whitelisted situations:
   - Confirming non-destructive continuation prompts (e.g., "continue", "y" for read-only inspection, pagination).
   - Instructing a stalled agent to run tests or linters after editing code.
   - NEVER use "AUTO_REPLY" if consecutive_auto_replies >= max_auto_replies.
3. Choose "ALERT_OPERATOR" if:
   - The agent asks to delete files, run destructive git commands (push, reset, force), install system packages, or change architecture.
   - The agent is stuck in a loop repeating the same error.
   - The agent has completed its main task and awaits final human review.

Always write "updated_summary" and "reason_for_operator" in concise Brazilian Portuguese (pt-BR)."#;

/// Builds the episodic user prompt JSON payload.
pub fn build_user_prompt_payload(state: &AgentDurableState, recent_hc_output: &str) -> Value {
    json!({
        "agent_state": {
            "agent_id": state.agent_id,
            "lifecycle_state": state.lifecycle_state.as_str(),
            "consecutive_auto_replies": state.consecutive_auto_replies,
            "max_auto_replies": state.max_auto_replies,
            "rolling_summary": state.rolling_summary,
        },
        "recent_hc_output": recent_hc_output,
    })
}

/// Returns the strict JSON Schema enforced via Ollama's `format` parameter.
pub fn output_json_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "updated_summary": {
                "type": "string",
                "description": "Resumo de 1 frase em português sobre o que o agente está fazendo agora."
            },
            "detected_state": {
                "type": "string",
                "enum": ["WORKING", "WAITING_CONFIRMATION", "STUCK_LOOP", "ERROR", "COMPLETED"]
            },
            "decision": {
                "type": "string",
                "enum": ["NO_OP", "AUTO_REPLY", "ALERT_OPERATOR"]
            },
            "urgency": {
                "type": "string",
                "enum": ["low", "normal", "critical"]
            },
            "hc_command_payload": {
                "type": "string",
                "description": "Texto exato a ser enviado para o agente via hc (vazio se NO_OP)."
            },
            "reason_for_operator": {
                "type": "string",
                "description": "Mensagem curta em português explicando por que a atenção do operador é necessária."
            }
        },
        "required": [
            "updated_summary",
            "detected_state",
            "decision",
            "urgency",
            "hc_command_payload",
            "reason_for_operator"
        ]
    })
}

/// Strips optional `<think>...</think>` blocks and markdown code fences, then parses a `CognitiveEvaluation` JSON string.
pub fn parse_evaluation_json(raw: &str) -> Result<CognitiveEvaluation> {
    let mut trimmed = raw.trim();
    if let Some(after_open) = trimmed.strip_prefix("<think>") {
        if let Some(end_idx) = after_open.find("</think>") {
            trimmed = after_open[end_idx + "</think>".len()..].trim();
        }
    }

    let stripped = if let Some(rest) = trimmed.strip_prefix("```") {
        let without_lang = rest
            .strip_prefix("json")
            .or_else(|| rest.strip_prefix("JSON"))
            .unwrap_or(rest);
        let without_leading_newline = without_lang.trim_start_matches(['\r', '\n']);
        without_leading_newline
            .strip_suffix("```")
            .unwrap_or(without_leading_newline)
            .trim()
    } else {
        trimmed
    };

    serde_json::from_str::<CognitiveEvaluation>(stripped)
        .with_context(|| format!("Failed to deserialize CognitiveEvaluation from: {stripped}"))
}

/// Returns the OpenAI-compatible chat completions path for a given base host URL.
pub fn openai_chat_completions_path(host: &str) -> &'static str {
    if host.trim_end_matches('/').ends_with("/v1") {
        "/chat/completions"
    } else {
        "/v1/chat/completions"
    }
}

/// Extracts `choices[0].message.content` from an OpenAI-compatible chat completions response.
pub fn extract_openai_chat_content(response: &Value) -> Result<&str> {
    response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .with_context(|| {
            format!("OpenAI-compatible response missing 'choices[0].message.content': {response}")
        })
}

/// Synchronous HTTP client for Ollama `/api/chat` and `/api/generate` endpoints.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    pub config: SupervisorConfig,
}

impl OllamaClient {
    pub fn new(config: SupervisorConfig) -> Self {
        Self { config }
    }

    /// Builds the `/api/chat` request payload with `num_gpu` policy and strict JSON schema.
    pub fn build_chat_request_body(
        &self,
        state: &AgentDurableState,
        recent_output: &str,
    ) -> Value {
        let user_payload = build_user_prompt_payload(state, recent_output);
        json!({
            "model": self.config.ollama_model,
            "stream": false,
            "format": output_json_schema(),
            "options": {
                "temperature": self.config.temperature,
                "num_gpu": self.config.effective_num_gpu(),
            },
            "messages": [
                {
                    "role": "system",
                    "content": SYSTEM_PROMPT,
                },
                {
                    "role": "user",
                    "content": user_payload.to_string(),
                }
            ]
        })
    }

    /// Builds an OpenAI-compatible `/v1/chat/completions` request payload with strict JSON schema.
    pub fn build_openai_chat_request_body(
        &self,
        state: &AgentDurableState,
        recent_output: &str,
    ) -> Value {
        let user_payload = build_user_prompt_payload(state, recent_output);
        json!({
            "model": self.config.ollama_model,
            "stream": false,
            "temperature": self.config.temperature,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "supervisor_evaluation",
                    "strict": true,
                    "schema": output_json_schema()
                }
            },
            "messages": [
                {
                    "role": "system",
                    "content": SYSTEM_PROMPT,
                },
                {
                    "role": "user",
                    "content": user_payload.to_string(),
                }
            ]
        })
    }

    /// Builds the `/api/generate` unload request payload (`keep_alive: 0`).
    pub fn build_unload_request_body(&self) -> Value {
        json!({
            "model": self.config.ollama_model,
            "keep_alive": 0,
        })
    }

    fn post_json_to_host(&self, host: &str, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", host.trim_end_matches('/'), path);
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(self.config.ollama_timeout_secs))
            .build();

        let mut req = agent.post(&url).set("Content-Type", "application/json");
        if let Some(key) = &self.config.api_key {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                req = req.set("Authorization", &format!("Bearer {trimmed}"));
            }
        }

        let response = req
            .send_json(body)
            .with_context(|| format!("LLM POST request to {url} failed"))?;

        response
            .into_json::<Value>()
            .with_context(|| format!("Failed to parse JSON response from {url}"))
    }

    fn post_json_with_fallback(&self, path: &str, body: &Value) -> Result<Value> {
        match self.post_json_to_host(&self.config.ollama_host, path, body) {
            Ok(val) => Ok(val),
            Err(primary_err) => {
                if let Some(fallback_host) = &self.config.ollama_fallback_host {
                    self.post_json_to_host(fallback_host, path, body)
                        .with_context(|| {
                            format!(
                                "Primary host ({}) failed: {primary_err}; fallback host ({fallback_host}) also failed",
                                self.config.ollama_host
                            )
                        })
                } else {
                    Err(primary_err)
                }
            }
        }
    }
}

impl CognitiveBackend for OllamaClient {
    fn evaluate(
        &self,
        state: &AgentDurableState,
        recent_output: &str,
    ) -> Result<CognitiveEvaluation> {
        match self.config.llm_backend {
            LlmBackendKind::Ollama => {
                let body = self.build_chat_request_body(state, recent_output);
                let response = self.post_json_with_fallback("/api/chat", &body)?;
                let content = response
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .with_context(|| {
                        format!("Ollama /api/chat response missing 'message.content': {response}")
                    })?;

                parse_evaluation_json(content)
            }
            LlmBackendKind::LlamaCpp
            | LlmBackendKind::LmStudio
            | LlmBackendKind::OpenAiCompatible => {
                let host = self.config.effective_primary_host();
                let path = openai_chat_completions_path(&host);
                let body = self.build_openai_chat_request_body(state, recent_output);
                let response = self.post_json_to_host(&host, path, &body)?;
                let content = extract_openai_chat_content(&response)?;
                parse_evaluation_json(content)
            }
        }
    }

    fn unload_model(&self) -> Result<()> {
        match self.config.llm_backend {
            LlmBackendKind::Ollama => {
                let body = self.build_unload_request_body();
                let _ = self.post_json_with_fallback("/api/generate", &body)?;
                Ok(())
            }
            LlmBackendKind::LlamaCpp
            | LlmBackendKind::LmStudio
            | LlmBackendKind::OpenAiCompatible => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> AgentDurableState {
        let mut state = AgentDurableState::new("worker-01", "refactor-auth", 2, 1_700_000_000);
        state.consecutive_auto_replies = 1;
        state.rolling_summary =
            "Executando testes unitários do módulo de autenticação.".to_string();
        state
    }

    #[test]
    fn test_build_user_prompt_payload_matches_spec() {
        let state = sample_state();
        let payload = build_user_prompt_payload(&state, "running 12 tests...\nContinue? [y/N]");

        let agent_state = payload.get("agent_state").expect("agent_state missing");
        assert_eq!(agent_state["agent_id"], "worker-01");
        assert_eq!(agent_state["lifecycle_state"], "WORKING");
        assert_eq!(agent_state["consecutive_auto_replies"], 1);
        assert_eq!(agent_state["max_auto_replies"], 2);
        assert_eq!(
            agent_state["rolling_summary"],
            "Executando testes unitários do módulo de autenticação."
        );
        assert_eq!(
            payload["recent_hc_output"],
            "running 12 tests...\nContinue? [y/N]"
        );
    }

    #[test]
    fn test_output_json_schema_has_all_required_properties() {
        let schema = output_json_schema();
        assert_eq!(schema["type"], "object");

        let props = schema["properties"]
            .as_object()
            .expect("properties should be an object");
        let required = schema["required"]
            .as_array()
            .expect("required should be an array");

        let expected_fields = [
            "updated_summary",
            "detected_state",
            "decision",
            "urgency",
            "hc_command_payload",
            "reason_for_operator",
        ];

        assert_eq!(props.len(), expected_fields.len());
        assert_eq!(required.len(), expected_fields.len());

        for field in expected_fields {
            assert!(props.contains_key(field), "missing property: {field}");
            assert!(
                required.iter().any(|v| v.as_str() == Some(field)),
                "missing required field: {field}"
            );
        }
    }

    #[test]
    fn test_parse_evaluation_json_valid_decisions() {
        let cases = [
            (
                r#"{
                    "updated_summary": "Compilando o projeto normalmente.",
                    "detected_state": "WORKING",
                    "decision": "NO_OP",
                    "urgency": "low",
                    "hc_command_payload": "",
                    "reason_for_operator": ""
                }"#,
                LifecycleState::Working,
                SupervisorDecision::NoOp,
                Urgency::Low,
            ),
            (
                r#"```json
                {
                    "updated_summary": "Aguardando confirmação para continuar leitura.",
                    "detected_state": "WAITING_CONFIRMATION",
                    "decision": "AUTO_REPLY",
                    "urgency": "normal",
                    "hc_command_payload": "y",
                    "reason_for_operator": "Confirmação segura de paginação."
                }
                ```"#,
                LifecycleState::WaitingConfirmation,
                SupervisorDecision::AutoReply,
                Urgency::Normal,
            ),
            (
                r#"{
                    "updated_summary": "Agente preso repetindo erro de permissão.",
                    "detected_state": "STUCK_LOOP",
                    "decision": "ALERT_OPERATOR",
                    "urgency": "critical",
                    "hc_command_payload": "",
                    "reason_for_operator": "Loop de erro detectado; intervenção humana necessária."
                }"#,
                LifecycleState::StuckLoop,
                SupervisorDecision::AlertOperator,
                Urgency::Critical,
            ),
        ];

        for (raw, expected_state, expected_decision, expected_urgency) in cases {
            let parsed = parse_evaluation_json(raw).expect("valid JSON should parse");
            assert_eq!(parsed.detected_state, expected_state);
            assert_eq!(parsed.decision, expected_decision);
            assert_eq!(parsed.urgency, expected_urgency);
        }
    }

    #[test]
    fn test_parse_evaluation_json_rejects_invalid_state() {
        let invalid = r#"{
            "updated_summary": "Estado desconhecido.",
            "detected_state": "UNKNOWN_STATE",
            "decision": "NO_OP",
            "urgency": "low",
            "hc_command_payload": "",
            "reason_for_operator": ""
        }"#;

        assert!(parse_evaluation_json(invalid).is_err());
    }

    #[test]
    fn test_request_payload_sets_num_gpu_zero_when_allow_gpu_false() {
        let mut config = SupervisorConfig::default();
        config.allow_gpu = false;
        config.gpu_layers = 99;
        let client = OllamaClient::new(config);
        let state = sample_state();

        let body = client.build_chat_request_body(&state, "cargo test");
        assert_eq!(body["options"]["num_gpu"], 0);
        assert_eq!(body["stream"], false);
        assert_eq!(body["format"], output_json_schema());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], SYSTEM_PROMPT);
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn test_build_openai_chat_request_body_structure() {
        let mut config = SupervisorConfig::default();
        config.llm_backend = LlmBackendKind::OpenAiCompatible;
        config.ollama_model = "openai/gpt-4o-mini".to_string();
        let client = OllamaClient::new(config);
        let state = sample_state();

        let body = client.build_openai_chat_request_body(&state, "cargo test");
        assert_eq!(body["model"], "openai/gpt-4o-mini");
        assert_eq!(body["stream"], false);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "supervisor_evaluation"
        );
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"],
            output_json_schema()
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["additionalProperties"],
            false
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], SYSTEM_PROMPT);
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn test_extract_openai_chat_content_and_think_stripping() {
        let response = json!({
            "choices": [
                {
                    "message": {
                        "content": "<think>\nAnalisando estado do agente...\n</think>\n```json\n{\"updated_summary\":\"Rodando testes.\",\"detected_state\":\"WORKING\",\"decision\":\"NO_OP\",\"urgency\":\"low\",\"hc_command_payload\":\"\",\"reason_for_operator\":\"\"}\n```"
                    }
                }
            ]
        });

        let content = extract_openai_chat_content(&response).expect("should extract content");
        let parsed = parse_evaluation_json(content).expect("should strip <think> and fences");
        assert_eq!(parsed.detected_state, LifecycleState::Working);
        assert_eq!(parsed.decision, SupervisorDecision::NoOp);
        assert_eq!(parsed.urgency, Urgency::Low);
        assert_eq!(parsed.updated_summary, "Rodando testes.");
    }

    #[test]
    fn test_openai_chat_completions_path() {
        assert_eq!(
            openai_chat_completions_path("http://localhost:8080"),
            "/v1/chat/completions"
        );
        assert_eq!(
            openai_chat_completions_path("https://openrouter.ai/api/v1"),
            "/chat/completions"
        );
        assert_eq!(
            openai_chat_completions_path("https://openrouter.ai/api/v1/"),
            "/chat/completions"
        );
    }
}
