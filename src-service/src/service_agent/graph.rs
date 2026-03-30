use std::collections::HashMap;

use super::types::ToolCall;

pub struct ToolNode {

    pub call: ToolCall,

    pub deps: Vec<String>,
}

pub struct ToolGraph {

    pub nodes: HashMap<String, ToolNode>,
}

impl ToolGraph {

    pub fn new() -> Self {

        Self {
            nodes: HashMap::new(),
        }
    }

    pub fn add_node(
        &mut self,
        id: String,
        call: ToolCall,
        deps: Vec<String>,
    ) {

        self.nodes.insert(
            id.clone(),
            ToolNode { call, deps },
        );
    }
}
