/// Built-in agent definitions seeded at app startup.
///
/// Each agent is upserted once into `agent_definitions` with `is_builtin = true`.
/// Existing records (matched by name + account_id) are left untouched so that
/// frontend edits to system prompts or tool_names are preserved across restarts.

use crate::db::SurrealDb;
use serde_json::json;

/// Upsert all built-in agent definitions for the given account.
///
/// Safe to call on every startup — uses a conditional INSERT that is a no-op
/// if the agent already exists.
pub(crate) async fn seed_builtin_agents(db: &SurrealDb, account_id: &str) {
    let agents = builtin_agents(account_id);
    for agent in agents {
        let name = agent["name"].as_str().unwrap_or("").to_string();
        if let Err(e) = db
            .query(
                "IF (SELECT count() FROM agent_definitions WHERE account_id = $aid AND name = $name GROUP ALL)[0].count = 0 \
                 THEN (CREATE agent_definitions CONTENT $data) \
                 END"
            )
            .bind(("aid",  account_id.to_string()))
            .bind(("name", name.clone()))
            .bind(("data", agent))
            .await
        {
            tracing::warn!("[seed_builtin_agents] upsert '{}' error: {}", name, e);
        }
    }
}

fn builtin_agents(account_id: &str) -> Vec<serde_json::Value> {
    vec![
        trace_analyst_def(account_id),
    ]
}

fn trace_analyst_def(account_id: &str) -> serde_json::Value {
    json!({
        "account_id":    account_id,
        "name":          "trace_analyst",
        "description":   "Analyses recent session traces and conversations to propose eval cases that capture guard invariants and behavioural patterns.",
        "kind":          "background",
        "is_active":     true,
        "is_builtin":    true,
        "enable_think":  true,
        "use_skill_pass": false,
        "tool_names": [
            "list_session_traces",
            "read_session_with_conversation",
            "propose_eval_case"
        ],
        "system_prompt": TRACE_ANALYST_PROMPT,
        "max_rounds":    10,
    })
}

const TRACE_ANALYST_PROMPT: &str = r#"
你是一個 Eval Case 提案專家，任務是分析最近的 session traces 和對話記錄，找出值得新增為 eval case 的行為模式。

## 你的工作流程

1. 呼叫 `list_session_traces` 取得最近的 sessions（建議 limit=10）
2. 針對每個感興趣的 session，呼叫 `read_session_with_conversation` 取得完整的 trace + 對話內容
3. 分析 trace 與對話，識別以下模式：
   - **Guard 正確攔截**：LLM 嘗試寫入但跳過了前置條件 → `Blocked` guard outcome
   - **Happy path 確認**：使用者意圖與 LLM 執行路徑完全吻合 → 值得作為正向範例
   - **Performance 異常**：簡單請求但 `round_count` 異常高 → `RoundCountLe` budget case
4. 針對每個有價值的模式，呼叫 `propose_eval_case` 儲存提案

## 判斷標準

**值得提案的情況：**
- trace 顯示某工具被 guard blocked，且對話中使用者的意圖確實需要先讀取才能寫入
- 一個完整的讀→寫序列在對話中被成功執行，可作為 happy path 範本
- `blocked_calls > 0` 且對話顯示 LLM 在第二輪自行修正了路徑（說明 guard 有效）

**不值得提案的情況：**
- trace 完全正常，沒有 guard block，也沒有特殊模式
- 對話內容是閒聊或簡單問答，沒有工具呼叫
- 已有相似名稱的 eval case 存在（避免重複）

## propose_eval_case 格式說明

`tool_sequence` 每項：
```json
{
  "id": "",
  "name": "tool_name",
  "args": { ... },
  "mock_result": "模擬的工具返回值"
}
```

`assertions` 使用 adjacently-tagged 格式：
```json
[
  { "type": "BlockedCountEq", "value": 1 },
  { "type": "GuardAt", "value": { "index": 0, "expected": "Blocked" } },
  { "type": "ToolAt", "value": { "index": 1, "name": "update_note" } },
  { "type": "NoBlockedCalls" },
  { "type": "RoundCountLe", "value": 3 }
]
```

`expected` 可為 `"Passed"` / `"Blocked"` / `"Exempt"`。

## 注意事項

- 每次分析最多提案 3 個 case，優先選擇最有代表性的
- `source_trace_ids` 填入觸發這個提案的 trace ID
- 提案 status 會自動設為 `pending_review`，需要人工在前端審核後啟用
- 不要提案涉及真實 vault 路徑或使用者個資的案例，使用通用路徑如 `notes/example.md`
"#;
