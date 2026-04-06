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

use crate::service::harness::runtime::HarnessRequestRuntime;
use crate::service::types::ToolFuture;

// ── Schemas ───────────────────────────────────────────────────────────────────

pub(crate) fn schema_list_session_traces() -> Value {
    json!({
        "name": "list_session_traces",
        "description": "List recent session traces for this account. Returns a summary of each session: trace_id, conv_id, started_at, round_count, blocked_calls, memory_facts_injected, skill_activations.",
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

pub(crate) fn handle_list_session_traces(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let limit = args["limit"].as_u64().unwrap_or(20).min(50) as usize;
        let mut resp = env.db
            .query(
                "SELECT meta::id(id) AS trace_id, conv_id, started_at, ended_at, \
                 round_count, skill_activations, memory_facts_injected, tool_calls \
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

        // Compute total_calls and blocked_calls in Rust to avoid SurrealQL array::filter issues.
        let summaries: Vec<Value> = rows.into_iter().map(|mut row| {
            let tool_calls = row["tool_calls"].as_array().cloned().unwrap_or_default();
            let total_calls = tool_calls.len();
            let blocked_calls = tool_calls.iter().filter(|t| {
                t["guard_outcome"]["type"].as_str() == Some("Blocked")
            }).count();
            // Remove raw tool_calls array from summary to keep payload small.
            if let Some(obj) = row.as_object_mut() {
                obj.remove("tool_calls");
                obj.insert("total_calls".to_string(),   json!(total_calls));
                obj.insert("blocked_calls".to_string(), json!(blocked_calls));
            }
            row
        }).collect();

        Ok(json!(summaries))
    })
}

pub(crate) fn handle_read_session_with_conversation(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
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

        // Load conversation messages via conv_id.
        // Conversations are stored as `conversations.messages_json` (a JSON string blob),
        // not a separate messages table.
        let conv_id = trace["conv_id"].as_str().unwrap_or("").to_string();
        let messages: Vec<Value> = if conv_id.is_empty() {
            vec![]
        } else {
            #[derive(serde::Deserialize)]
            struct ConvRow { messages_json: Option<String> }
            match env.db
                .query("SELECT messages_json FROM conversations WHERE record::id(id) = $cid LIMIT 1")
                .bind(("cid", conv_id.clone()))
                .await
            {
                Ok(mut r) => {
                    let rows: Vec<ConvRow> = r.take(0).unwrap_or_default();
                    rows.into_iter().next()
                        .and_then(|row| row.messages_json)
                        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|m| m["role"].as_str() != Some("system"))
                        .collect()
                }
                Err(_) => vec![],
            }
        };

        Ok(json!({ "trace": trace, "messages": messages }))
    })
}

pub(crate) fn handle_propose_eval_case(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            return Ok(json!({"error": "name is required"}));
        }

        // DB-level dedup: reject if a case with the same name already exists for this account.
        // Prevents trace_analyst from creating duplicate proposals on repeated runs.
        #[derive(serde::Deserialize)]
        struct CountRow { count: i64 }
        let exists = env.db
            .query("SELECT count() AS count FROM proposed_eval_cases \
                    WHERE account_id = $aid AND name = $name GROUP ALL")
            .bind(("aid",  env.account_id.clone()))
            .bind(("name", name.clone()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<CountRow>>(0).ok())
            .and_then(|rows| rows.into_iter().next().map(|r| r.count > 0))
            .unwrap_or(false);

        if exists {
            return Ok(json!({ "ok": false, "reason": "duplicate", "name": name,
                              "message": "A case with this name already exists — skipped to avoid duplicates." }));
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
