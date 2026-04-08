/// Tools for the `trace_analyst` agent.
///
/// These tools are NOT in the standard vault tool set — they are only available
/// to agents whose `tool_names` explicitly includes them. The `trace_analyst`
/// seed agent uses all six to form its analysis loop:
///
/// 1. `list_session_traces`            — discover recent sessions
/// 2. `read_session_with_conversation` — load full context for one session
/// 3. `propose_eval_case`              — save a proposed case to `proposed_eval_cases`
/// 4. `list_proposed_eval_cases`       — see existing proposals + status + last_run_result
/// 5. `run_eval_case`                  — run a single approved/enabled case, see pass/fail
/// 6. `search_traces_by_pattern`       — filter traces by blocked_calls, round_count, etc.

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

pub(crate) fn schema_list_proposed_eval_cases() -> Value {
    json!({
        "name": "list_proposed_eval_cases",
        "description": "List eval cases you have previously proposed, including their status (pending_review/enabled/disabled) and last run result. Use this to avoid duplicate proposals and to check whether your cases are passing.",
        "parameters": {
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "description": "Filter by status: 'pending_review', 'enabled', 'disabled'. Omit to return all."
                }
            },
            "required": []
        }
    })
}

pub(crate) fn schema_run_eval_case() -> Value {
    json!({
        "name": "run_eval_case",
        "description": "Run a single eval case by name or case_id and return pass/fail with failure details. Use this to verify that a proposed case correctly catches the pattern it was designed for.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The exact name of the eval case (from list_proposed_eval_cases)."
                }
            },
            "required": ["name"]
        }
    })
}

pub(crate) fn schema_search_traces_by_pattern() -> Value {
    json!({
        "name": "search_traces_by_pattern",
        "description": "Filter session traces by behavioural patterns. More precise than list_session_traces for finding problematic sessions.",
        "parameters": {
            "type": "object",
            "properties": {
                "min_blocked_calls": {
                    "type": "integer",
                    "description": "Only return traces with at least this many blocked tool calls."
                },
                "min_round_count": {
                    "type": "integer",
                    "description": "Only return traces with at least this many rounds (high round count = potential stall)."
                },
                "min_repeated_calls": {
                    "type": "integer",
                    "description": "Only return traces where repeated_call_count >= this value."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 10, max 30)."
                }
            },
            "required": []
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

pub(crate) fn handle_list_proposed_eval_cases(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let status_filter = args["status"].as_str().map(String::from);
        let mut resp = if let Some(ref status) = status_filter {
            env.db
                .query("SELECT meta::id(id) AS case_id, name, description, status, source, \
                        last_run_result, last_run_at, source_trace_ids \
                        FROM proposed_eval_cases \
                        WHERE account_id = $aid AND status = $status \
                        ORDER BY last_run_at DESC")
                .bind(("aid",    env.account_id.clone()))
                .bind(("status", status.clone()))
                .await
                .map_err(|e| format!("list_proposed_eval_cases error: {}", e))?
        } else {
            env.db
                .query("SELECT meta::id(id) AS case_id, name, description, status, source, \
                        last_run_result, last_run_at, source_trace_ids \
                        FROM proposed_eval_cases \
                        WHERE account_id = $aid \
                        ORDER BY last_run_at DESC")
                .bind(("aid", env.account_id.clone()))
                .await
                .map_err(|e| format!("list_proposed_eval_cases error: {}", e))?
        };
        let rows: Vec<Value> = resp.take(0).unwrap_or_default();
        Ok(json!(rows))
    })
}

pub(crate) fn handle_run_eval_case(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let name = args["name"].as_str().unwrap_or("").trim().to_string();
        if name.is_empty() {
            return Ok(json!({"error": "name is required"}));
        }

        let mut resp = env.db
            .query("SELECT * FROM proposed_eval_cases \
                    WHERE account_id = $aid AND name = $name LIMIT 1")
            .bind(("aid",  env.account_id.clone()))
            .bind(("name", name.clone()))
            .await
            .map_err(|e| format!("run_eval_case query error: {}", e))?;

        let rows: Vec<Value> = resp.take(0).unwrap_or_default();
        let row = match rows.into_iter().next() {
            Some(r) => r,
            None => return Ok(json!({"error": format!("eval case '{}' not found", name)})),
        };

        let case: crate::service::harness::eval::EvalCase = match serde_json::from_value(row.clone()) {
            Ok(c) => c,
            Err(e) => return Ok(json!({"error": format!("failed to deserialise case: {}", e)})),
        };

        let result = crate::service::harness::eval::EvalRunner::run(&case).await;
        let now = chrono::Utc::now().timestamp();
        let passed = result.passed();
        let result_json = json!({
            "passed":   passed,
            "failures": result.failures,
        });

        // Persist last_run_result back to DB.
        let _ = env.db
            .query("UPDATE proposed_eval_cases SET last_run_result = $res, last_run_at = $now \
                    WHERE account_id = $aid AND name = $name")
            .bind(("res",  result_json.clone()))
            .bind(("now",  now))
            .bind(("aid",  env.account_id.clone()))
            .bind(("name", name.clone()))
            .await;

        Ok(json!({
            "name":    name,
            "passed":  passed,
            "result":  result_json,
        }))
    })
}

pub(crate) fn handle_search_traces_by_pattern(env: Arc<HarnessRequestRuntime>, args: Value) -> ToolFuture {
    Box::pin(async move {
        let min_blocked   = args["min_blocked_calls"].as_u64().unwrap_or(0);
        let min_rounds    = args["min_round_count"].as_u64().unwrap_or(0);
        let min_repeated  = args["min_repeated_calls"].as_u64().unwrap_or(0);
        let limit         = args["limit"].as_u64().unwrap_or(10).min(30) as usize;

        let mut resp = env.db
            .query(
                "SELECT meta::id(id) AS trace_id, conv_id, started_at, ended_at, \
                 round_count, repeated_call_count, skill_activations, \
                 memory_facts_injected, tool_calls \
                 FROM session_traces \
                 WHERE account_id = $aid \
                 ORDER BY started_at DESC \
                 LIMIT 200"  // over-fetch then filter in Rust (SurrealQL array math is limited)
            )
            .bind(("aid", env.account_id.clone()))
            .await
            .map_err(|e| format!("search_traces_by_pattern error: {}", e))?;

        let rows: Vec<Value> = resp.take(0).unwrap_or_default();

        let matched: Vec<Value> = rows.into_iter().filter_map(|mut row| {
            let tool_calls = row["tool_calls"].as_array().cloned().unwrap_or_default();
            let blocked_count = tool_calls.iter().filter(|t| {
                t["guard_outcome"]["type"].as_str() == Some("Blocked")
            }).count() as u64;
            let round_count   = row["round_count"].as_u64().unwrap_or(0);
            let repeated      = row["repeated_call_count"].as_u64().unwrap_or(0);

            if blocked_count < min_blocked || round_count < min_rounds || repeated < min_repeated {
                return None;
            }

            if let Some(obj) = row.as_object_mut() {
                obj.remove("tool_calls");
                obj.insert("blocked_calls".to_string(),   json!(blocked_count));
                obj.insert("repeated_calls".to_string(),  json!(repeated));
            }
            Some(row)
        }).take(limit).collect();

        if matched.is_empty() {
            Ok(json!("No traces matched the given pattern criteria."))
        } else {
            Ok(json!(matched))
        }
    })
}
