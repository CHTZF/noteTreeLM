use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

/// Generic non-streaming LLM agent loop.
///
/// - Sends `messages` to the LLM, executes tool calls via `dispatch_tool`, appends results, repeats.
/// - Stops when LLM returns no tool calls, `max_rounds` is reached, or `cancel` is set.
/// - Returns the last non-empty text response from the LLM.
///
/// `dispatch_tool(name, args)` → `Result<Value, String>`
pub async fn run_basic_loop<F, Fut>(
    client: &reqwest::Client,
    llm_url: &str,
    mut messages: Vec<Value>,
    tools_schema: Option<Value>,
    max_rounds: usize,
    cancel: Option<Arc<AtomicBool>>,
    dispatch_tool: F,
) -> String
where
    F: Fn(String, Value) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let cancel = cancel.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let mut last_text = String::new();

    for _round in 0..max_rounds {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let (content, tool_chunks) = match super::super::tools::vault_tools::call_llm_once(
            client, llm_url, &messages, tools_schema.clone(), &cancel,
        ).await {
            Ok(r)  => r,
            Err(_) => break,
        };

        if !content.is_empty() {
            last_text = content.clone();
        }

        if tool_chunks.is_empty() {
            break;
        }

        // Append assistant message with tool_calls
        let tc_json: Vec<Value> = tool_chunks.iter().map(|(id, name, args)| json!({
            "id": id, "type": "function",
            "function": { "name": name, "arguments": args },
        })).collect();
        messages.push(json!({ "role": "assistant", "content": Value::Null, "tool_calls": tc_json }));

        // Execute tools sequentially, append tool role messages
        for (tc_id, fn_name, fn_args_str) in &tool_chunks {
            let fn_args: Value = serde_json::from_str(fn_args_str).unwrap_or(json!({}));
            let result = dispatch_tool(fn_name.clone(), fn_args).await;
            let result_str = match &result {
                Ok(v)  => serde_json::to_string(v).unwrap_or_default(),
                Err(e) => format!("ERROR: {}", e),
            };
            messages.push(json!({
                "role": "tool",
                "tool_call_id": tc_id,
                "content": result_str,
            }));
        }
    }

    last_text
}
