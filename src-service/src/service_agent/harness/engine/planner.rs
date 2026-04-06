use super::graph::ToolGraph;
use crate::service_agent::types::ToolCall;

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
