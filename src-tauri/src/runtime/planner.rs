use super::graph::ToolGraph;
use super::types::ToolCall;

use serde_json::Value;

/// 解析 LLM 以文字格式輸出的工具調用
/// 支援格式：<tool_call>{"name":"func","arguments":{...}}</tool_call>
pub(crate) fn parse_text_tool_calls(content: &str) -> Vec<Value> {
    let mut calls = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find("<tool_call>") {
        let after_open = &remaining[start + "<tool_call>".len()..];
        if let Some(end) = after_open.find("</tool_call>") {
            let json_str = after_open[..end].trim();
            if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                let name = v["name"].as_str().unwrap_or("").to_string();
                let args_str = serde_json::to_string(&v["arguments"])
                    .unwrap_or_else(|_| "{}".to_string());
                calls.push(serde_json::json!({
                    "id": format!("call_{}", name),
                    "type": "function",
                    "function": { "name": name, "arguments": args_str }
                }));
            }
            remaining = &after_open[end + "</tool_call>".len()..];
        } else {
            break;
        }
    }
    calls
}

pub struct Planner;

impl Planner {
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
}