use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::executor::Executor;
use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use super::super::types::{EmitEventFn, IsWriteFn};
use crate::state::ToolCallRecord;

pub type ToolCallStore = Arc<tokio::sync::Mutex<HashMap<String, ToolCallRecord>>>;

pub struct Dispatcher {
    registry: Arc<ToolRegistry>,
    emit_fn: EmitEventFn,
    is_write_fn: IsWriteFn,
    tool_calls: ToolCallStore,
}

impl Dispatcher {

    pub fn new(
        registry: Arc<ToolRegistry>,
        emit_fn: EmitEventFn,
        is_write_fn: IsWriteFn,
        tool_calls: ToolCallStore,
    ) -> Self {
        Self { registry, emit_fn, is_write_fn, tool_calls }
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
            Arc::clone(&self.tool_calls),
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
            Arc::clone(&self.tool_calls),
        );
        executor.execute_graph_stream(graph, tx)
    }
}
