use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::executor::Executor;
use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;

pub struct Dispatcher {

    registry: Arc<ToolRegistry>,
}

impl Dispatcher {

    pub fn new(registry: Arc<ToolRegistry>) -> Self {

        Self { registry }
    }

    /// 同步執行（tx 生命週期由 Agent 管理）
    pub async fn run(
        &self,
        tx: Arc<Transaction>,
        graph: ToolGraph,
    ) -> Result<Vec<Value>, String> {

        let executor = Executor::new(Arc::clone(&self.registry));

        executor.execute_graph(graph, tx).await
    }

    /// Streaming 執行（tx 生命週期由 Agent 管理）
    /// 注意：此函數本身不是 async，不做任何 await
    pub fn run_stream(
        &self,
        tx: Arc<Transaction>,
        graph: ToolGraph,
    ) -> mpsc::Receiver<Result<Value, String>> {

        let executor = Executor::new(Arc::clone(&self.registry));

        executor.execute_graph_stream(graph, tx)
    }
}
