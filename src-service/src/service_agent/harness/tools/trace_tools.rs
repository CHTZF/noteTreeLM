/// Tools for the `trace_analyst` agent.
///
/// These tools are NOT in the standard vault tool set — they are only available
/// to agents whose `tool_names` explicitly includes them. The `trace_analyst`
/// seed agent uses all three to form its analysis loop:
///
/// 1. `list_session_traces`          — discover recent sessions
/// 2. `read_session_with_conversation` — load full context for one session
/// 3. `propose_eval_case`            — save a proposed case to `proposed_eval_cases`

use std::sync::Arc;
use serde_json::{json, Value};

use crate::service_agent::harness::env::VaultEnv;
use crate::service_agent::types::ToolFuture;

// ── Schemas ───────────────────────────────────────────────────────────────────

pub(crate) fn schema_list_session_traces() -> Value {
    json!({
        "name": "list_session_traces",
        "description": "List recent session traces for this account. Returns a summary of each session: trace_id, conv_id, started_at, round_count, blocked_calls, skill_activations.",
        "parameters": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of recent traces to return (default 20, max 50)."
                }
            },
            "required": []
        }
    })
}

pub(crate) fn schema_read_session_with_conversation() -> Value {
    json!({
        "name": "read_session_with_conversation",
        "description": "Load the full SessionTrace and the associated conversation messages for a specific session. Use the trace_id from list_session_traces. Returns {trace, messages} where messages is the episodic conversation history.",
        "parameters": {
            "type": "object",
            "properties": {
                "trace_id": {
                    "type": "string",
                    "description": "The record ID of the session trace (from list_session_traces)."
                }
            },
            "required": ["trace_id"]
        }
    })
}

pub(crate) fn schema_propose_eval_case() -> Value {
    json!({
        "name": "propose_eval_case",
        "description": "Save a proposed eval case to the proposed_eval_cases table for human review. Status will be 'pending_review' until approved via the frontend. Provide a clear name, the tool sequence the agent should execute, and the assertions that verify correct behaviour.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Short descriptive name for the eval case (e.g. 'update without read — blocked')."
                },
                "description": {
                    "type": "string",
                    "description": "Explanation of what pattern this case tests and why it was identified."
                },
                "tool_sequence": {
                    "type": "array",
                    "description": "Ordered list of mock tool calls.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name":        { "type": "string" },
                            "args":        { "type": "object" },
                            "mock_result": {}
                        },
                        "required": ["name", "args", "mock_result"]
                    }
                },
                "assertions": {
                    "type": "array",
                    "description": "TraceAssertion list. Each item: {\"type\": \"BlockedCountEq\", \"value\": 1} or {\"type\": \"NoBlockedCalls\"} etc.",
                    "items": { "type": "object" }
                },
                "source_trace_ids": {
                    "type": "array",
                    "description": "Trace IDs that inspired this proposal.",
                    "items": { "type": "string" }
                }
            },
            "required": ["name", "description", "tool_sequence", "assertions"]
        }
    })
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub(crate) fn handle_list_session_traces(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let limit = args["limit"].as_u64().unwrap_or(20).min(50);
        let mut resp = env.db
            .query(
                "SELECT meta::id(id) AS trace_id, conv_id, started_at, ended_at, \
                 round_count, skill_activations, \
                 array::len(tool_calls) AS total_calls, \
                 array::len(array::filter(tool_calls, |$t| $t.guard_outcome.type = 'Blocked')) AS blocked_calls \
                 FROM session_traces \
                 WHERE account_id = $aid \
                 ORDER BY started_at DESC \
                 LIMIT $limit"
            )
            .bind(("aid",   env.account_id.clone()))
            .bind(("limit", limit))
            .await
            .map_err(|e| format!("list_session_traces query error: {}", e))?;

        let rows: Vec<Value> = resp.take(0).unwrap_or_default();
        Ok(json!(rows))
    })
}

pub(crate) fn handle_read_session_with_conversation(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let trace_id = args["trace_id"].as_str().unwrap_or("").trim().to_string();
        if trace_id.is_empty() {
            return Ok(json!({"error": "trace_id is required"}));
        }

        // Load trace
        let mut resp = env.db
            .query("SELECT * FROM session_traces WHERE meta::id(id) = $tid AND account_id = $aid LIMIT 1")
            .bind(("tid", trace_id.clone()))
            .bind(("aid", env.account_id.clone()))
            .await
            .map_err(|e| format!("read trace error: {}", e))?;

        let traces: Vec<Value> = resp.take(0).unwrap_or_default();
        let trace = match traces.into_iter().next() {
            Some(t) => t,
            None => return Ok(json!({"error": format!("trace '{}' not found", trace_id)})),
        };

        // Load conversation messages via conv_id
        let conv_id = trace["conv_id"].as_str().unwrap_or("").to_string();
        let messages: Vec<Value> = if conv_id.is_empty() {
            vec![]
        } else {
            match env.db
                .query("SELECT role, content FROM messages WHERE conversation_id = $cid ORDER BY created_at ASC LIMIT 100")
                .bind(("cid", conv_id.clone()))
                .await
            {
                Ok(mut r) => r.take(0).unwrap_or_default(),
                Err(_)    => vec![],
            }
        };

        Ok(json!({ "trace": trace, "messages": messages }))
    })
}

pub(crate) fn handle_propose_eval_case(env: Arc<VaultEnv>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            return Ok(json!({"error": "name is required"}));
        }

        let payload = json!({
            "account_id":       env.account_id,
            "name":             name,
            "description":      args["description"].as_str().unwrap_or(""),
            "tool_sequence":    args["tool_sequence"],
            "assertions":       args["assertions"],
            "source_trace_ids": args["source_trace_ids"].as_array().cloned().unwrap_or_default(),
            "source":           "llm_proposed",
            "status":           "pending_review",
            "last_run_result":  serde_json::Value::Null,
            "last_run_at":      serde_json::Value::Null,
        });

        env.db
            .query("CREATE proposed_eval_cases CONTENT $data")
            .bind(("data", payload))
            .await
            .map_err(|e| format!("propose_eval_case write error: {}", e))?;

        Ok(json!({ "ok": true, "name": name, "status": "pending_review" }))
    })
}
