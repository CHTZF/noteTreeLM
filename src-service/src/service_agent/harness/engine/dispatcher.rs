use std::sync::Arc;

use serde_json::Value;

use super::executor::Executor;
use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use crate::service_agent::types::{EmitEventFn, NeedConfirmFn};
use crate::service_agent::harness::runtime::HarnessRequestRuntime;

#[derive(Clone)]
pub struct Dispatcher {
    registry:        Arc<ToolRegistry>,
    emit_fn:         EmitEventFn,
    need_confirm_fn: NeedConfirmFn,
}

impl Dispatcher {

    pub(crate) fn new(
        registry:        Arc<ToolRegistry>,
        emit_fn:         EmitEventFn,
        need_confirm_fn: NeedConfirmFn,
    ) -> Self {
        Self { registry, emit_fn, need_confirm_fn }
    }

    pub async fn run(
        &self,
        env: Arc<HarnessRequestRuntime>,
        tx: Arc<Transaction>,
        graph: ToolGraph,
    ) -> Result<Vec<Value>, String> {
        let executor = Executor::new(
            Arc::clone(&self.registry),
            env,
            Arc::clone(&self.emit_fn),
            Arc::clone(&self.need_confirm_fn),
        );
        executor.execute_graph(graph, tx).await
    }
}
