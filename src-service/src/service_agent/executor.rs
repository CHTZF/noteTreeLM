use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::{Transaction, TransactionState};
use super::types::{EmitEventFn, IsWriteFn, ToolCall};

pub struct Executor {
    registry: Arc<ToolRegistry>,
    emit_fn: EmitEventFn,
    is_write_fn: IsWriteFn,
}

impl Executor {

    pub fn new(
        registry: Arc<ToolRegistry>,
        emit_fn: EmitEventFn,
        is_write_fn: IsWriteFn,
    ) -> Self {
        Self { registry, emit_fn, is_write_fn }
    }

    pub async fn execute_graph(
        &self,
        graph: ToolGraph,
        tx: Arc<Transaction>,
    ) -> Result<Vec<Value>, String> {

        let mut completed = HashSet::new();
        let mut executed: Vec<ToolCall> = Vec::new();
        let mut results = Vec::new();

        'outer: loop {
            // Cancel check at the start of each DAG traversal pass
            if tx.state().await == TransactionState::Cancelled {
                break;
            }

            let mut progress = false;
            let mut should_cancel = false;

            for (id, node) in &graph.nodes {
                if completed.contains(id) {
                    continue;
                }
                if !node.deps.iter().all(|d| completed.contains(d)) {
                    continue;
                }

                // ── Pre-execute: emit SSE + write confirmation ──────────────
                let approved = if (self.is_write_fn)(&node.call.name) {
                    (self.emit_fn)(
                        "agent:write_request".to_string(),
                        json!({ "display": node.call.name }),
                    );
                    match tx.prepare().await {
                        Ok(rx) => {
                            match tokio::time::timeout(Duration::from_secs(120), rx).await {
                                Ok(Ok(v)) => v,
                                // Timeout or channel dropped: cancel transaction + notify frontend
                                _ => {
                                    let _ = tx.cancel().await;
                                    (self.emit_fn)(
                                        "agent:write_timeout".to_string(),
                                        json!({ "display": node.call.name }),
                                    );
                                    should_cancel = true;
                                    break;
                                }
                            }
                        }
                        Err(_) => {
                            // Transaction already Cancelled
                            should_cancel = true;
                            break;
                        }
                    }
                } else {
                    (self.emit_fn)(
                        "agent:tool_call".to_string(),
                        json!({ "display": node.call.name }),
                    );
                    true
                };

                if !approved {
                    // Write rejected by user
                    results.push(json!(
                        "❌ 使用者拒絕了此操作。請詢問使用者是否需要調整操作內容，或是否還有其他需要協助的事項。"
                    ));
                    completed.insert(id.clone());
                    progress = true;
                    continue;
                }

                // ── Execute ─────────────────────────────────────────────────
                let tool = self.registry
                    .get(&node.call.name)
                    .ok_or_else(|| format!("tool not found: {}", node.call.name))?;

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

            if should_cancel || !progress {
                break 'outer;
            }
        }

        Ok(results)
    }

    async fn rollback(&self, executed: Vec<ToolCall>) {
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
        let emit_fn = Arc::clone(&self.emit_fn);
        let is_write_fn = Arc::clone(&self.is_write_fn);

        tokio::spawn(async move {
            let mut completed = HashSet::new();
            let mut executed: Vec<ToolCall> = Vec::new();

            'outer: loop {
                if tx.state().await == TransactionState::Cancelled {
                    break;
                }

                let mut progress = false;
                let mut should_cancel = false;

                for (id, node) in &graph.nodes {
                    if completed.contains(id) { continue; }
                    if !node.deps.iter().all(|d| completed.contains(d)) { continue; }

                    let approved = if is_write_fn(&node.call.name) {
                        emit_fn("agent:write_request".to_string(), json!({ "display": node.call.name }));
                        match tx.prepare().await {
                            Ok(rx_confirm) => {
                                match tokio::time::timeout(Duration::from_secs(120), rx_confirm).await {
                                    Ok(Ok(v)) => v,
                                    _ => {
                                        let _ = tx.cancel().await;
                                        emit_fn("agent:write_timeout".to_string(), json!({ "display": node.call.name }));
                                        should_cancel = true;
                                        break;
                                    }
                                }
                            }
                            Err(_) => { should_cancel = true; break; }
                        }
                    } else {
                        emit_fn("agent:tool_call".to_string(), json!({ "display": node.call.name }));
                        true
                    };

                    if !approved {
                        let _ = sender.send(Ok(json!(
                            "❌ 使用者拒絕了此操作。"
                        ))).await;
                        completed.insert(id.clone());
                        progress = true;
                        continue;
                    }

                    if let Some(tool) = registry.get(&node.call.name) {
                        let result = (tool.execute)(node.call.args.clone()).await;
                        match result {
                            Ok(v) => {
                                tx.record_tool(&node.call.name).await;
                                let _ = sender.send(Ok(v.clone())).await;
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

                if should_cancel || !progress { break 'outer; }
            }
        });

        rx
    }
}
