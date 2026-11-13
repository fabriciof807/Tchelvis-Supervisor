use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

use super::config::SupervisorConfig;
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

/// Strips optional markdown code fences and parses a `CognitiveEvaluation` JSON string.
pub fn parse_evaluation_json(raw: &str) -> Result<CognitiveEvaluation> {
    let trimmed = raw.trim();
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

        let response = agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_json(body)
            .with_context(|| format!("Ollama POST request to {url} failed"))?;

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

    fn unload_model(&self) -> Result<()> {
        let body = self.build_unload_request_body();
        let _ = self.post_json_with_fallback("/api/generate", &body)?;
        Ok(())
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
}
