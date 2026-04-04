use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::graph::ToolGraph;
use super::tool_registry::ToolRegistry;
use super::transaction::{Transaction, TransactionState};
use super::super::types::{EmitEventFn, IsWriteFn, ToolCall};
use super::super::harness::memory::working::WorkingMemory;
use crate::state::GuardOutcome;
use crate::service_agent::harness::governance::policy::GUARD_EXEMPT_WRITE_TOOLS;

pub struct Executor {
    registry: Arc<ToolRegistry>,
    emit_fn: EmitEventFn,
    is_write_fn: IsWriteFn,
    working_memory: WorkingMemory,
}

impl Executor {

    pub(crate) fn new(
        registry: Arc<ToolRegistry>,
        emit_fn: EmitEventFn,
        is_write_fn: IsWriteFn,
        working_memory: WorkingMemory,
    ) -> Self {
        Self { registry, emit_fn, is_write_fn, working_memory }
    }

    pub async fn execute_graph(
        &self,
        graph: ToolGraph,
        tx: Arc<Transaction>,
    ) -> Result<Vec<Value>, String> {

        let mut completed: HashSet<String> = HashSet::new();
        let mut executed: Vec<ToolCall> = Vec::new();
        let mut results = Vec::new();
        // Track (result, args) by node id for arg_resolver
        let mut node_results: std::collections::HashMap<String, (Value, Value)> = std::collections::HashMap::new();

        // Topological order: collect node ids sorted by insertion order via dep count
        // (graph is always a linear chain for meta-functions, DAG for LLM multi-tool)
        let sorted_ids = topo_sort(&graph);

        'outer: loop {
            if tx.state().await == TransactionState::Cancelled {
                break;
            }

            let mut progress = false;
            let mut should_cancel = false;

            for id in &sorted_ids {
                let node = match graph.nodes.get(id) {
                    Some(n) => n,
                    None => continue,
                };
                if completed.contains(id) { continue; }
                if !node.deps.iter().all(|d| completed.contains(d)) { continue; }

                // Resolve args via resolver if present
                let actual_args = if let Some(ref resolver) = node.arg_resolver {
                    let (prev_result, prev_args) = node.resolver_dep.as_ref()
                        .and_then(|dep| node_results.get(dep))
                        .map(|(r, a)| (r.clone(), a.clone()))
                        .unwrap_or((Value::Null, Value::Null));
                    resolver(&prev_result, &prev_args)
                } else {
                    node.call.args.clone()
                };

                // ── Chain gate (plan_announce): emit plan, await confirmation ──
                if node.is_chain_gate {
                    let plan = actual_args["plan"].as_str()
                        .unwrap_or(node.call.args["plan"].as_str().unwrap_or("即將執行操作"));
                    (self.emit_fn)(
                        "agent:write_request".to_string(),
                        json!({ "display": plan }),
                    );
                    match tx.prepare().await {
                        Err(_) => { should_cancel = true; break; }
                        Ok(rx) => match tokio::time::timeout(Duration::from_secs(120), rx).await {
                            Ok(Ok(true)) => {
                                tx.pre_approve();
                                completed.insert(id.clone());
                                node_results.insert(id.clone(), (Value::Null, Value::Null));
                                progress = true;
                                continue;
                            }
                            _ => { let _ = tx.cancel().await; should_cancel = true; break; }
                        }
                    }
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
                        Err(_) => { should_cancel = true; break; }
                    }
                } else {
                    if node.call.name == "think" {
                        (self.emit_fn)(
                            "agent:think".to_string(),
                            json!({ "thought": actual_args["thought"].as_str().unwrap_or("") }),
                        );
                    } else {
                        (self.emit_fn)(
                            "agent:tool_call".to_string(),
                            json!({ "display": node.call.name }),
                        );
                    }
                    true
                };

                if !approved {
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

                let started_at = chrono::Utc::now().timestamp();
                let t0 = Instant::now();
                let result = (tool.execute)(actual_args.clone()).await;
                let duration_ms = t0.elapsed().as_millis() as u64;

                match result {
                    Ok(v) => {
                        tx.record_tool(&node.call.name).await;
                        // Detect guard-blocked sentinel; unwrap to plain hint for LLM.
                        let (guard_outcome, v) = extract_guard_outcome(v, &node.call.name);
                        // Record in per-session working memory for citation + path guard verification.
                        self.working_memory.record(
                            node.call.id.clone(),
                            node.call.name.clone(),
                            actual_args.clone(),
                            v.clone(),
                            started_at,
                            duration_ms,
                            guard_outcome,
                        ).await;
                        // Annotate result with cite_id so LLM can reference it in final reply.
                        let v_cited = annotate_cite_id(v, &node.call.id);
                        // Empty search result mid-chain: abort chain, return fallback sentinel.
                        let has_dependents = graph.nodes.values()
                            .any(|n| n.deps.contains(id));
                        if has_dependents && is_search_tool(&node.call.name)
                            && v_cited.as_array().map(|a| a.is_empty()).unwrap_or(false)
                        {
                            results.push(json!("__chain_empty__"));
                            should_cancel = true;
                            break;
                        }
                        node_results.insert(id.clone(), (v_cited.clone(), actual_args));
                        results.push(v_cited);
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

        let tool_calls = self.working_memory.clone();
        tokio::spawn(async move {
            let mut completed: HashSet<String> = HashSet::new();
            let mut executed: Vec<ToolCall> = Vec::new();
            let mut node_results: std::collections::HashMap<String, (Value, Value)> = std::collections::HashMap::new();
            let sorted_ids = topo_sort(&graph);

            'outer: loop {
                if tx.state().await == TransactionState::Cancelled { break; }

                let mut progress = false;
                let mut should_cancel = false;

                for id in &sorted_ids {
                    let node = match graph.nodes.get(id) {
                        Some(n) => n,
                        None => continue,
                    };
                    if completed.contains(id) { continue; }
                    if !node.deps.iter().all(|d| completed.contains(d)) { continue; }

                    let actual_args = if let Some(ref resolver) = node.arg_resolver {
                        let (prev_result, prev_args) = node.resolver_dep.as_ref()
                            .and_then(|dep| node_results.get(dep))
                            .map(|(r, a)| (r.clone(), a.clone()))
                            .unwrap_or((Value::Null, Value::Null));
                        resolver(&prev_result, &prev_args)
                    } else {
                        node.call.args.clone()
                    };

                    // Chain gate
                    if node.is_chain_gate {
                        let plan = actual_args["plan"].as_str()
                            .unwrap_or(node.call.args["plan"].as_str().unwrap_or("即將執行操作"));
                        emit_fn("agent:write_request".to_string(), json!({ "display": plan }));
                        match tx.prepare().await {
                            Err(_) => { should_cancel = true; break; }
                            Ok(rx_confirm) => match tokio::time::timeout(Duration::from_secs(120), rx_confirm).await {
                                Ok(Ok(true)) => {
                                    tx.pre_approve();
                                    completed.insert(id.clone());
                                    node_results.insert(id.clone(), (Value::Null, Value::Null));
                                    progress = true;
                                    continue;
                                }
                                _ => { let _ = tx.cancel().await; should_cancel = true; break; }
                            }
                        }
                    }

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
                        if node.call.name == "think" {
                            emit_fn("agent:think".to_string(), json!({ "thought": actual_args["thought"].as_str().unwrap_or("") }));
                        } else {
                            emit_fn("agent:tool_call".to_string(), json!({ "display": node.call.name }));
                        }
                        true
                    };

                    if !approved {
                        let _ = sender.send(Ok(json!("❌ 使用者拒絕了此操作。"))).await;
                        completed.insert(id.clone());
                        progress = true;
                        continue;
                    }

                    if let Some(tool) = registry.get(&node.call.name) {
                        let started_at = chrono::Utc::now().timestamp();
                        let t0 = Instant::now();
                        let result = (tool.execute)(actual_args.clone()).await;
                        let duration_ms = t0.elapsed().as_millis() as u64;
                        match result {
                            Ok(v) => {
                                tx.record_tool(&node.call.name).await;
                                // Detect guard-blocked sentinel; unwrap to plain hint for LLM.
                                let (guard_outcome, v) = extract_guard_outcome(v, &node.call.name);
                                // Record in per-session working memory.
                                tool_calls.record(
                                    node.call.id.clone(),
                                    node.call.name.clone(),
                                    actual_args.clone(),
                                    v.clone(),
                                    started_at,
                                    duration_ms,
                                    guard_outcome,
                                ).await;
                                let v_cited = annotate_cite_id(v, &node.call.id);
                                node_results.insert(id.clone(), (v_cited.clone(), actual_args));
                                let _ = sender.send(Ok(v_cited.clone())).await;
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

/// Kahn's algorithm topological sort for ToolGraph.
/// Returns node ids in execution order (deps before dependents).
fn topo_sort(graph: &ToolGraph) -> Vec<String> {
    use std::collections::VecDeque;
    let mut in_degree: std::collections::HashMap<&str, usize> = graph.nodes
        .keys()
        .map(|id| (id.as_str(), 0))
        .collect();
    for node in graph.nodes.values() {
        for dep in &node.deps {
            *in_degree.entry(dep.as_str()).or_insert(0) += 0; // ensure dep key exists
            if let Some(cnt) = in_degree.get_mut(dep.as_str()) {
                let _ = cnt; // dep already counted; we need in-degree of node, not dep
            }
        }
    }
    // Re-compute: in_degree[id] = number of OTHER nodes that list id as dep (i.e. id's out-degree)
    // Actually we want: in_degree[id] = number of deps id has (= node.deps.len())
    let mut in_deg: std::collections::HashMap<String, usize> = graph.nodes
        .iter()
        .map(|(id, node)| (id.clone(), node.deps.len()))
        .collect();
    let mut queue: VecDeque<String> = in_deg.iter()
        .filter(|(_, &d)| d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    // Stable order within same in-degree level: sort queue by id for determinism
    let mut q_vec: Vec<String> = queue.drain(..).collect();
    q_vec.sort();
    queue.extend(q_vec);

    let mut order = Vec::new();
    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        // find nodes whose dep list contains this id
        let mut newly_ready: Vec<String> = graph.nodes.iter()
            .filter(|(nid, node)| !order.contains(nid) && node.deps.contains(&id))
            .filter_map(|(nid, _)| {
                let d = in_deg.get_mut(nid)?;
                *d -= 1;
                if *d == 0 { Some(nid.clone()) } else { None }
            })
            .collect();
        newly_ready.sort();
        queue.extend(newly_ready);
    }
    // Fallback: if graph had cycles (shouldn't happen), append remaining
    for id in graph.nodes.keys() {
        if !order.contains(id) { order.push(id.clone()); }
    }
    order
}

fn is_search_tool(name: &str) -> bool {
    matches!(name, "search_vault" | "list_structure")
}

/// Detect the `__guard_blocked__` sentinel injected by the registry execute closure,
/// and classify the outcome for `WorkingMemory::record()`.
///
/// | Result value                        | GuardOutcome returned        |
/// |-------------------------------------|------------------------------|
/// | `{ "__guard_blocked__": true, … }`  | `Blocked(hint)`              |
/// | normal value, tool is exempt        | `Exempt`                     |
/// | normal value, tool has guard → pass | `Passed`                     |
///
/// `tool_name` is used to identify exempt tools (those in `GUARD_EXEMPT_WRITE_TOOLS`)
/// so `GuardOutcome::Exempt` is emitted instead of `Passed` for creation-style tools.
fn extract_guard_outcome(v: Value, tool_name: &str) -> (GuardOutcome, Value) {
    if v.get("__guard_blocked__").and_then(|b| b.as_bool()).unwrap_or(false) {
        let hint = v["__guard_hint__"].as_str().unwrap_or("guard blocked").to_string();
        (GuardOutcome::Blocked(hint.clone()), Value::String(hint))
    } else if GUARD_EXEMPT_WRITE_TOOLS.contains(&tool_name) {
        (GuardOutcome::Exempt, v)
    } else {
        (GuardOutcome::Passed, v)
    }
}

/// Inject `__cite_id__` into a tool result so LLM can reference it in the final reply.
/// - Object → add field directly.
/// - Array  → wrap as `{ "__cite_id__": id, "items": [...] }`.
/// - String → wrap as `{ "__cite_id__": id, "content": "..." }`.
/// - Other  → return as-is (e.g. write-tool confirmations are not cited).
fn annotate_cite_id(v: Value, cite_id: &str) -> Value {
    match v {
        Value::Object(mut map) => {
            map.insert("__cite_id__".to_string(), json!(cite_id));
            Value::Object(map)
        }
        Value::Array(arr) => json!({ "__cite_id__": cite_id, "items": arr }),
        Value::String(s)  => json!({ "__cite_id__": cite_id, "content": s }),
        other => other,
    }
}
