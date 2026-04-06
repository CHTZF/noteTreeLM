use std::sync::Arc;

use serde_json::Value;

use super::executor::Executor;
use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use super::super::types::{EmitEventFn, NeedConfirmFn};
use super::super::harness::memory::working::WorkingMemory;

pub struct Dispatcher {
    registry: Arc<ToolRegistry>,
    emit_fn: EmitEventFn,
    need_confirm_fn: NeedConfirmFn,
    working_memory: WorkingMemory,
}

impl Dispatcher {

    pub(crate) fn new(
        registry: Arc<ToolRegistry>,
        emit_fn: EmitEventFn,
        need_confirm_fn: NeedConfirmFn,
        working_memory: WorkingMemory,
    ) -> Self {
        Self { registry, emit_fn, need_confirm_fn, working_memory }
    }

    pub async fn run(
        &self,
        tx: Arc<Transaction>,
        graph: ToolGraph,
    ) -> Result<Vec<Value>, String> {
        let executor = Executor::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.emit_fn),
            Arc::clone(&self.need_confirm_fn),
            self.working_memory.clone(),
        );
        executor.execute_graph(graph, tx).await
    }
}
