use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::executor::Executor;
use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use super::types::{EmitEventFn, IsWriteFn};

pub struct Dispatcher {
    registry: Arc<ToolRegistry>,
    emit_fn: EmitEventFn,
    is_write_fn: IsWriteFn,
}

impl Dispatcher {

    pub fn new(
        registry: Arc<ToolRegistry>,
        emit_fn: EmitEventFn,
        is_write_fn: IsWriteFn,
    ) -> Self {
        Self { registry, emit_fn, is_write_fn }
    }

    /// 同步執行（tx 生命週期由 Agent 管理）
    pub async fn run(
        &self,
        tx: Arc<Transaction>,
        graph: ToolGraph,
    ) -> Result<Vec<Value>, String> {
        let executor = Executor::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.emit_fn),
            Arc::clone(&self.is_write_fn),
        );
        executor.execute_graph(graph, tx).await
    }

    /// Streaming 執行（tx 生命週期由 Agent 管理）
    pub fn run_stream(
        &self,
        tx: Arc<Transaction>,
        graph: ToolGraph,
    ) -> mpsc::Receiver<Result<Value, String>> {
        let executor = Executor::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.emit_fn),
            Arc::clone(&self.is_write_fn),
        );
        executor.execute_graph_stream(graph, tx)
    }
}
