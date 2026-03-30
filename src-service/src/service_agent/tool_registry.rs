use std::collections::HashMap;

use super::types::Tool;

pub struct ToolRegistry {

    tools: HashMap<String, Tool>,
}

impl ToolRegistry {

    pub fn new() -> Self {

        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(
        &mut self,
        name: String,
        tool: Tool,
    ) {

        self.tools.insert(name, tool);
    }

    pub fn get(&self, name: &str) -> Option<&Tool> {

        self.tools.get(name)
    }
}
