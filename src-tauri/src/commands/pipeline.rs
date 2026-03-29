/// commands/pipeline.rs
///
/// Tool Pipeline 執行：
///   PipelineStep / PipelineStepResult / VaultChangedPayload
///   cancel_tool_test / topo_sort_indices / run_tool_pipeline / test_vault_tool

use crate::{error::AppError, state::AppState};
use crate::runtime::tool_dispatch::{execute_vault_tool, is_write_tool};
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

// ── Pipeline 型別（用於 run_tool_pipeline） ───────────────────────────────

#[derive(serde::Deserialize)]
pub struct PipelineStep {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    /// 前置步驟 ID 列表（相依必須先執行完畢）
    pub deps: Vec<String>,
}

#[derive(serde::Serialize)]
pub struct PipelineStepResult {
    pub id: String,
    pub name: String,
    pub ok: bool,
    /// true = 取消旗標觸發，此步驟未執行
    pub cancelled: bool,
    pub output: String,
    pub duration_ms: u64,
}

/// vault:changed 事件 payload（寫入工具 commit 後 emit，觸發前端 sidebar + editor 刷新）
#[derive(serde::Serialize, Clone)]
pub struct VaultChangedPayload {
    pub creates: Vec<String>,
    pub updates: Vec<String>,
}

/// 取消進行中的工具測試台 Pipeline
#[tauri::command]
pub async fn cancel_tool_test(state: State<'_, AppState>) -> Result<(), AppError> {
    state.tool_test_cancel.store(true, Ordering::Relaxed);
    Ok(())
}

/// Kahn's 演算法拓撲排序，返回 steps 陣列的執行索引順序
fn topo_sort_indices(steps: &[PipelineStep]) -> Vec<usize> {
    use std::collections::{HashMap, VecDeque};
    let id_to_idx: HashMap<&str, usize> = steps.iter().enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let n = steps.len();
    let mut in_degree = vec![0usize; n];
    let mut successors: Vec<Vec<usize>> = vec![vec![]; n];

    for (i, step) in steps.iter().enumerate() {
        for dep_id in &step.deps {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                in_degree[i] += 1;
                successors[dep_idx].push(i);
            }
        }
    }

    let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut ordered = Vec::with_capacity(n);

    while let Some(i) = queue.pop_front() {
        ordered.push(i);
        for &j in &successors[i] {
            in_degree[j] -= 1;
            if in_degree[j] == 0 {
                queue.push_back(j);
            }
        }
    }

    // 有環路時，其餘步驟依原始順序追加
    if ordered.len() < n {
        for i in 0..n {
            if !ordered.contains(&i) {
                ordered.push(i);
            }
        }
    }

    ordered
}

/// 依 Planner ToolGraph 相容格式執行多工具 Pipeline，供 debug 測試台使用
///
/// Transaction 語意：
/// - 開始前 emit `agent:tx_debug` kind="prepare"
/// - 每步驟執行前檢查取消旗標；若已取消，剩餘步驟標記 cancelled=true 並 emit "cancel"
/// - 全部完成後 emit kind="commit"
/// - 有寫入工具時 emit `vault:changed`（觸發前端 sidebar + editor 刷新）
#[tauri::command]
pub async fn run_tool_pipeline(
    app: AppHandle,
    state: State<'_, AppState>,
    steps: Vec<PipelineStep>,
) -> Result<Vec<PipelineStepResult>, AppError> {
    // 重置取消旗標（新 pipeline 開始）
    state.tool_test_cancel.store(false, Ordering::Relaxed);

    let session_id = Uuid::new_v4().to_string();
    let vault_path = state.get_vault_path().await;

    let all_tool_names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();

    // ── Emit prepare ─────────────────────────────────────────────────────────
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": "prepare",
        "tools": all_tool_names,
    }));

    let order = topo_sort_indices(&steps);
    let mut results: Vec<PipelineStepResult> = Vec::with_capacity(steps.len());
    let mut executed_names: Vec<String> = Vec::new();
    let mut vault_creates: Vec<String> = Vec::new();
    let mut vault_updates: Vec<String> = Vec::new();

    for idx in order {
        let step = &steps[idx];

        // ── 取消檢查（每步執行前）─────────────────────────────────────────
        if state.tool_test_cancel.load(Ordering::Relaxed) {
            results.push(PipelineStepResult {
                id: step.id.clone(),
                name: step.name.clone(),
                ok: false,
                cancelled: true,
                output: String::new(),
                duration_ms: 0,
            });
            continue;
        }

        let start = std::time::Instant::now();
        let output = execute_vault_tool(
            &step.name,
            &step.args,
            &vault_path,
            &app,
        ).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        // 記錄寫入路徑以便 commit 後 emit vault:changed
        match step.name.as_str() {
            "create_note" | "create_folder" => {
                if let Some(p) = step.args["path"].as_str() {
                    vault_creates.push(p.to_string());
                }
            }
            "update_note" => {
                if let Some(p) = step.args["path"].as_str() {
                    vault_updates.push(p.to_string());
                }
            }
            _ => {}
        }

        executed_names.push(step.name.clone());
        results.push(PipelineStepResult {
            id: step.id.clone(),
            name: step.name.clone(),
            ok: true,
            cancelled: false,
            output,
            duration_ms,
        });
    }

    // ── Emit commit 或 cancel ────────────────────────────────────────────────
    let was_cancelled = state.tool_test_cancel.load(Ordering::Relaxed);
    let kind = if was_cancelled { "cancel" } else { "commit" };
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": kind,
        "tools": executed_names,
    }));

    // ── vault:changed（commit 且有寫入操作）──────────────────────────────────
    if !was_cancelled && (!vault_creates.is_empty() || !vault_updates.is_empty()) {
        let _ = app.emit("vault:changed", VaultChangedPayload {
            creates: vault_creates,
            updates: vault_updates,
        });
    }

    Ok(results)
}

/// 直接測試單一 Agent 工具，供 debug 面板使用
///
/// Transaction 語意：
/// - 開始前 emit `agent:tx_debug` kind="prepare"
/// - 執行完畢 emit kind="commit"（若旗標已被取消則 emit "cancel"）
/// - 寫入工具 commit 後 emit `vault:changed`（觸發前端 sidebar + editor 刷新）
#[tauri::command]
pub async fn test_vault_tool(
    app: AppHandle,
    state: State<'_, AppState>,
    tool_name: String,
    args: serde_json::Value,
) -> Result<String, AppError> {
    // 重置取消旗標（新測試開始）
    state.tool_test_cancel.store(false, Ordering::Relaxed);

    let session_id = Uuid::new_v4().to_string();
    let vault_path = state.get_vault_path().await;

    // ── Emit prepare ─────────────────────────────────────────────────────────
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": "prepare",
        "tools": [tool_name.clone()],
    }));

    let result = execute_vault_tool(
        &tool_name,
        &args,
        &vault_path,
        &app,
    ).await;

    // ── Emit commit 或 cancel ────────────────────────────────────────────────
    let was_cancelled = state.tool_test_cancel.load(Ordering::Relaxed);
    let kind = if was_cancelled { "cancel" } else { "commit" };
    let _ = app.emit("agent:tx_debug", serde_json::json!({
        "session_id": session_id,
        "kind": kind,
        "tools": [tool_name.clone()],
    }));

    // ── vault:changed（commit 且為寫入工具）──────────────────────────────────
    if !was_cancelled && is_write_tool(&tool_name) {
        let path = args["path"].as_str().unwrap_or("").to_string();
        let (creates, updates) = match tool_name.as_str() {
            "update_note" => (vec![], vec![path]),
            _ => (vec![path], vec![]),
        };
        let _ = app.emit("vault:changed", VaultChangedPayload { creates, updates });
    }

    Ok(result)
}
