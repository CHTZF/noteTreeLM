use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use super::super::types::ToolCall;

/// Derives this node's args from the previous (effective) node's result and args.
/// Signature: (prev_result, prev_args) → args_for_this_node
pub type ArgResolver = Arc<dyn Fn(&Value, &Value) -> Value + Send + Sync>;

pub struct ToolNode {
    pub call: ToolCall,
    pub deps: Vec<String>,
    /// Which node's result/args to pass into `arg_resolver`. May skip `plan_announce`
    /// nodes since they produce no meaningful result.
    pub resolver_dep: Option<String>,
    /// If Some, called at execution time to fill `call.args` from the resolver_dep's
    /// result and args. If None, `call.args` is used as-is.
    pub arg_resolver: Option<ArgResolver>,
    /// True for `plan_announce` steps in a chain: Executor emits `agent:write_request`,
    /// calls `tx.prepare()`, and calls `tx.pre_approve()` on confirmation — without
    /// dispatching to the tool registry.
    pub is_chain_gate: bool,
}

impl ToolNode {
    pub fn new(call: ToolCall, deps: Vec<String>) -> Self {
        Self { call, deps, resolver_dep: None, arg_resolver: None, is_chain_gate: false }
    }
}

pub struct ToolGraph {
    pub nodes: HashMap<String, ToolNode>,
}

impl ToolGraph {
    pub fn new() -> Self {
        Self { nodes: HashMap::new() }
    }

    pub fn add_node(&mut self, id: String, call: ToolCall, deps: Vec<String>) {
        self.nodes.insert(id.clone(), ToolNode::new(call, deps));
    }

    pub fn add_node_full(&mut self, id: String, node: ToolNode) {
        self.nodes.insert(id, node);
    }
}
