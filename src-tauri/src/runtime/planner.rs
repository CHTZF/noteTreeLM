use super::graph::ToolGraph;
use super::types::ToolCall;

use serde_json::Value;

pub struct Planner;

impl Planner {

    pub fn plan(
        llm_output: Value,
    ) -> ToolGraph {

        let mut graph = ToolGraph::new();

        let tools = llm_output
            .as_array()
            .unwrap();

        let mut prev: Option<String> = None;

        for t in tools {

            let id = t["id"].as_str().unwrap().to_string();

            let name = t["name"].as_str().unwrap().to_string();

            let args = t["args"].clone();

            let deps = match &prev {

                Some(p) => vec![p.clone()],

                None => vec![],
            };

            graph.add_node(
                id.clone(),
                ToolCall {
                    id: id.clone(),
                    name,
                    args,
                },
                deps,
            );

            prev = Some(id);
        }

        graph
    }
}