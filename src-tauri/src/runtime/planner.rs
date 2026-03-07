use super::graph::ToolGraph;
use super::types::ToolCall;

use serde_json::Value;

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