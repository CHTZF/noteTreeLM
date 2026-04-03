use std::sync::Arc;

use super::graph::{ToolGraph, ToolNode, ArgResolver};
use super::super::types::ToolCall;
use super::super::agents::interactive::{extract_user_tool_args, extract_chain_step_args};

use serde_json::{json, Value};

pub struct Planner;

impl Planner {
    /// 將 stream_llm_round 回傳的 tool_chunks（Vec<(id, name, arguments_str)>）
    /// 解析 arguments_str 並轉成循序 ToolGraph。
    pub fn plan_from_chunks(tool_chunks: &[(String, String, String)]) -> ToolGraph {
        let calls: Vec<(String, String, Value)> = tool_chunks.iter()
            .map(|(id, name, args_str)| {
                let args = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
                (id.clone(), name.clone(), args)
            })
            .collect();
        Self::plan(&calls)
    }

    /// 將 LLM 回傳的工具呼叫清單轉成循序 ToolGraph（A→B→C dep chain）。
    ///
    /// 文字格式的 tool calls id 可能為空，此處補上 `call_{i}` 確保 HashMap key 唯一。
    pub fn plan(tool_calls: &[(String, String, Value)]) -> ToolGraph {
        let mut graph = ToolGraph::new();
        let mut prev: Option<String> = None;

        for (i, (id, name, args)) in tool_calls.iter().enumerate() {
            let eid = if id.is_empty() { format!("call_{}", i) } else { id.clone() };
            let deps = prev.iter().cloned().collect();
            graph.add_node(
                eid.clone(),
                ToolCall { id: eid.clone(), name: name.clone(), args: args.clone() },
                deps,
            );
            prev = Some(eid);
        }

        graph
    }

    /// Build a deterministic linear ToolGraph from a skill chain and the LLM-supplied args.
    ///
    /// - chain[0]: args from `extract_user_tool_args`; no resolver needed.
    /// - `plan_announce` nodes: `is_chain_gate = true`; plan text derived from
    ///   remaining chain steps; no resolver_dep.
    /// - chain[i>0] (non-gate): arg_resolver calls `extract_chain_step_args` using
    ///   the previous *effective* node's result/args (skips plan_announce nodes).
    pub fn plan_from_meta_function(chain: &[String], user_args: &Value) -> ToolGraph {
        let mut graph = ToolGraph::new();
        let mut prev_effective_id: Option<String> = None;
        let mut prev_effective_tool: Option<String> = None;

        for (i, tool_name) in chain.iter().enumerate() {
            let node_id = format!("chain_{}", i);

            if tool_name == "plan_announce" {
                // Build plan text from remaining non-gate steps
                let remaining: Vec<&str> = chain[i + 1..].iter()
                    .filter(|t| t.as_str() != "plan_announce")
                    .map(|s| s.as_str())
                    .collect();
                let plan_text = if remaining.is_empty() {
                    "即將執行操作".to_string()
                } else {
                    format!("即將執行：{}", remaining.join(" → "))
                };
                let deps = prev_effective_id.iter().cloned().collect();
                let node = ToolNode {
                    call: ToolCall {
                        id: node_id.clone(),
                        name: tool_name.clone(),
                        args: json!({ "plan": plan_text }),
                    },
                    deps,
                    resolver_dep: None,
                    arg_resolver: None,
                    is_chain_gate: true,
                };
                graph.add_node_full(node_id.clone(), node);
                // plan_announce does NOT update prev_effective_id/tool
                // (it has no result; deps chain still needs to pass through it)
                // but we DO make it a dep for the next node
                prev_effective_id = Some(node_id);
                // prev_effective_tool stays the same
                continue;
            }

            if i == 0 || prev_effective_tool.is_none() {
                // First real tool: args come from LLM user_args
                let args = extract_user_tool_args(tool_name, user_args);
                let node = ToolNode {
                    call: ToolCall { id: node_id.clone(), name: tool_name.clone(), args },
                    deps: prev_effective_id.iter().cloned().collect(),
                    resolver_dep: None,
                    arg_resolver: None,
                    is_chain_gate: false,
                };
                graph.add_node_full(node_id.clone(), node);
            } else {
                // Subsequent real tool: args derived from previous effective tool's result
                let from_tool = prev_effective_tool.clone().unwrap();
                let to_tool = tool_name.clone();
                let user_args_c = user_args.clone();
                let resolver: ArgResolver = Arc::new(move |prev_result: &Value, prev_args: &Value| {
                    extract_chain_step_args(&from_tool, &to_tool, prev_result, prev_args, &user_args_c)
                });
                let deps: Vec<String> = prev_effective_id.iter().cloned().collect();
                let node = ToolNode {
                    call: ToolCall {
                        id: node_id.clone(),
                        name: tool_name.clone(),
                        args: Value::Null,
                    },
                    deps,
                    resolver_dep: prev_effective_id.clone(),
                    arg_resolver: Some(resolver),
                    is_chain_gate: false,
                };
                graph.add_node_full(node_id.clone(), node);
            }

            prev_effective_id = Some(node_id);
            prev_effective_tool = Some(tool_name.clone());
        }

        graph
    }

    /// 將 Dispatcher 執行結果配對回 tool_chunks，產生 OpenAI `role: tool` messages。
    /// 與 `plan_from_chunks` 搭配使用，確保 tool_call_id 對應正確。
    pub fn results_to_messages(
        tool_chunks: &[(String, String, String)],
        results: Vec<Value>,
    ) -> Vec<Value> {
        tool_chunks.iter().enumerate().map(|(i, (tc_id, tc_name, _))| {
            let result_val = results.get(i).cloned().unwrap_or(json!("ERROR: no result"));
            let content = match result_val {
                Value::String(s) => s,
                other => serde_json::to_string(&other).unwrap_or_default(),
            };
            json!({
                "role": "tool",
                "tool_call_id": tc_id,
                "name": tc_name,
                "content": content,
            })
        }).collect()
    }
}
