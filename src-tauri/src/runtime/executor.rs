use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::Transaction;
use super::types::ToolCall;

pub struct Executor {

    registry: Arc<ToolRegistry>,
}

impl Executor {

    pub fn new(registry: Arc<ToolRegistry>) -> Self {

        Self { registry }
    }

    pub async fn execute_graph(
        &self,
        graph: ToolGraph,
        tx: Arc<Transaction>,
    ) -> Result<Vec<Value>, String> {

        let mut completed = HashSet::new();
        let mut executed: Vec<ToolCall> = Vec::new();
        let mut results = Vec::new();

        loop {

            let mut progress = false;

            for (id, node) in &graph.nodes {

                if completed.contains(id) {
                    continue;
                }

                if node.deps.iter().all(|d| completed.contains(d)) {

                    let tool = self.registry
                        .get(&node.call.name)
                        .ok_or("tool not found")?;

                    let result = (tool.execute)(node.call.args.clone()).await;

                    match result {

                        Ok(v) => {
                            tx.record_tool(&node.call.name).await;
                            results.push(v);
                            completed.insert(id.clone());
                            executed.push(node.call.clone());
                            progress = true;
                        }

                        Err(e) => {
                            self.rollback(executed).await;
                            return Err(e);
                        }
                    }
                }
            }

            if !progress {
                break;
            }
        }

        Ok(results)
    }

    async fn rollback(
        &self,
        executed: Vec<ToolCall>,
    ) {

        for call in executed.into_iter().rev() {

            if let Some(tool) = self.registry.get(&call.name) {

                if let Some(rb) = &tool.rollback {

                    let _ = rb(call.args.clone()).await;

                }
            }
        }
    }

    pub fn execute_graph_stream(
        &self,
        graph: ToolGraph,
        tx: Arc<Transaction>,
    ) -> mpsc::Receiver<Result<Value, String>> {
        let (sender, rx) = mpsc::channel(10);
        let registry = Arc::clone(&self.registry);

        tokio::spawn(async move {
            let mut completed = HashSet::new();
            let mut executed: Vec<ToolCall> = Vec::new();

            loop {
                let mut progress = false;

                for (id, node) in &graph.nodes {
                    if completed.contains(id) {
                        continue;
                    }

                    if node.deps.iter().all(|d| completed.contains(d)) {
                        if let Some(tool) = registry.get(&node.call.name) {
                            let result = (tool.execute)(node.call.args.clone()).await;

                            match result {
                                Ok(v) => {
                                    tx.record_tool(&node.call.name).await;
                                    let _ = sender.send(Ok(v)).await;
                                    completed.insert(id.clone());
                                    executed.push(node.call.clone());
                                    progress = true;
                                }
                                Err(e) => {
                                    for call in executed.into_iter().rev() {
                                        if let Some(t) = registry.get(&call.name) {
                                            if let Some(rb) = &t.rollback {
                                                let _ = rb(call.args.clone()).await;
                                            }
                                        }
                                    }
                                    let _ = sender.send(Err(e)).await;
                                    return;
                                }
                            }
                        } else {
                            let _ = sender.send(Err(format!("Tool not found: {}", node.call.name))).await;
                            return;
                        }
                    }
                }

                if !progress {
                    break;
                }
            }
        });

        rx
    }
}
