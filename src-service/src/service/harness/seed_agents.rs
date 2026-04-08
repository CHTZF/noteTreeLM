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
    let now = chrono::Utc::now().timestamp();
    let agents = builtin_agents(account_id, now);
    for agent in agents {
        let name    = agent["name"].as_str().unwrap_or("").to_string();
        let prompt  = agent["system_prompt"].as_str().unwrap_or("").to_string();
        let version = agent["system_prompt_version"].as_u64().unwrap_or(1);

        if let Err(e) = db
            .query(
                // Create if new; update system_prompt when version in code exceeds DB version.
                // Other fields (tool_names, max_rounds, etc.) are left untouched so user
                // edits made via the frontend are preserved.
                "IF (SELECT count() FROM agent_definitions \
                     WHERE account_id = $aid AND name = $name GROUP ALL)[0].count = 0 \
                 THEN (CREATE agent_definitions CONTENT $data) \
                 ELSE IF (SELECT VALUE system_prompt_version FROM agent_definitions \
                          WHERE account_id = $aid AND name = $name \
                          LIMIT 1)[0] < $ver \
                 THEN (UPDATE agent_definitions \
                       SET system_prompt = $prompt, system_prompt_version = $ver \
                       WHERE account_id = $aid AND name = $name) \
                 END"
            )
            .bind(("aid",    account_id.to_string()))
            .bind(("name",   name.clone()))
            .bind(("data",   agent))
            .bind(("prompt", prompt))
            .bind(("ver",    version))
            .await
        {
            tracing::warn!("[seed_builtin_agents] upsert '{}' error: {}", name, e);
        }
    }
}

fn builtin_agents(account_id: &str, now: i64) -> Vec<serde_json::Value> {
    vec![
        trace_analyst_def(account_id, now),
        memory_agent_def(account_id, now),
        kb_query_def(account_id, now),
    ]
}

fn kb_query_def(account_id: &str, now: i64) -> serde_json::Value {
    json!({
        "account_id":    account_id,
        "def_id":        "builtin_kb_query",
        "name":          "kb_query",
        "description":   "知識庫問答 agent：搜尋已匯入頁面並回答問題，支援 cite_id 引用追蹤。",
        "kind":          "chat",
        "is_active":     true,
        "is_builtin":    true,
        "enable_think":  false,
        "use_skill_pass": false,
        "skill_ids":     ["builtin_kb_search"],
        "tool_names":    ["search_kb_pages"],
        "trigger":       "",
        "status":        "active",
        "use_count":     0,
        "created_at":    now,
        "system_prompt": "你是知識庫問答助理。使用 search_kb_pages 工具搜尋已匯入的知識頁面，根據結果回答使用者問題。\n\
            ## 規則\n\
            1. 先呼叫 search_kb_pages（query 填使用者問題）取得相關頁面（含 __cite_id__ 欄位）。\n\
            2. 回答的第一句必須以 [cite:id1,id2] 格式標明所依據的 cite_id（例如 [cite:kb_1,kb_2]）；若未使用任何工具則輸出 [cite:none]。\n\
            3. 若結果為空，回覆「目前沒有相關的知識點。請嘗試重新表述問題，或到管理來源手動匯入更多頁面。」\n\
            4. 可進行比較、對比、綜合分析；若引用多個來源，以結構化方式呈現。",
        "system_prompt_version": 1,
        "max_rounds":    3,
    })
}

fn trace_analyst_def(account_id: &str, now: i64) -> serde_json::Value {
    json!({
        "account_id":    account_id,
        "def_id":        "builtin_trace_analyst",
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
            "propose_eval_case",
            "list_proposed_eval_cases",
            "run_eval_case",
            "update_proposed_eval_case",
            "search_traces_by_pattern"
        ],
        "skill_ids":             [],
        "trigger":               "",
        "status":                "active",
        "use_count":             0,
        "created_at":            now,
        "system_prompt":         TRACE_ANALYST_PROMPT,
        "system_prompt_version": 4,
        "max_rounds":            12,
    })
}

fn memory_agent_def(account_id: &str, now: i64) -> serde_json::Value {
    json!({
        "account_id":    account_id,
        "def_id":        "builtin_memory_agent",
        "name":          "memory_agent",
        "description":   "定期掃描未分析的對話，萃取長期記憶事實並整理到記憶庫",
        "kind":          "scheduled",
        "is_active":     true,
        "is_builtin":    true,
        "enable_think":  false,
        "use_skill_pass": false,
        "tool_names": [
            "get_unprocessed_conversations",
            "get_conversation_content",
            "save_memory_facts",
            "mark_conversation_processed",
            "condense_memory_facts"
        ],
        "skill_ids":             [],
        "trigger":               "memory_agent",
        "status":                "active",
        "use_count":             0,
        "created_at":            now,
        "system_prompt":         MEMORY_AGENT_PROMPT,
        "system_prompt_version": 1,
        "max_rounds":            20,
    })
}

const MEMORY_AGENT_PROMPT: &str = "你是記憶管理助理。\n\
    ## 模式判斷\n\
    若使用者訊息中包含「conv_id:」，進入 **單一對話模式**：\n\
      1. 從訊息中解析 conv_id 與 skip_count（若有 skip_count: 欄位，解析為數字，預設 0）\n\
      2. 呼叫 get_conversation_content（帶入 conversation_id 與 skip_count），跳過 get_unprocessed_conversations\n\
      3. 判斷是否有長期記憶價值\n\
      4. 有價值 → 呼叫 save_memory_facts；記錄回傳的 facts_saved 數字\n\
      5. 呼叫 mark_conversation_processed\n\
      6. 僅當 facts_saved > 0 時，呼叫 condense_memory_facts（不傳 category）\n\
    \n\
    若使用者訊息中不含「conv_id:」，進入 **全量掃描模式**：\n\
      1. 呼叫 get_unprocessed_conversations 取得待分析對話列表\n\
      2. 對每個對話，以 processed_msg_count 作為 skip_count 呼叫 get_conversation_content（只讀尚未處理的新訊息）\n\
      3. 判斷是否有長期記憶價值（使用者偏好/背景/規則/重要決策）\n\
         - 一般查詢、搜尋、閒聊 → 無記憶價值\n\
         - 使用者分享個人資訊、設定偏好、做重要決定 → 有記憶價值\n\
      4. 有價值 → 呼叫 save_memory_facts；累計 facts_saved 總數\n\
      5. 每個對話無論成功失敗 → 呼叫 mark_conversation_processed\n\
      6. 所有對話處理完畢後，僅當累計 facts_saved > 0 時，呼叫 condense_memory_facts（不傳 category）\n\
    \n\
    完成後輸出摘要（處理幾個對話、儲存幾條記憶）。";

const TRACE_ANALYST_PROMPT: &str = r#"
你是一個 Eval Case 提案專家，任務是分析最近的 session traces 和對話記錄，找出值得新增為 eval case 的行為模式。

## 你的工作流程

1. 先呼叫 `list_proposed_eval_cases` 了解已有哪些提案（避免重複），確認哪些 enabled case 最近 pass/fail
2. 用 `search_traces_by_pattern` 快速找出有問題的 session（例如 `min_blocked_calls=1` 或 `min_round_count=5`）
3. 針對感興趣的 session，呼叫 `read_session_with_conversation` 取得完整的 trace + 對話內容
4. 分析 trace 與對話，識別以下模式：
   - **Guard 正確攔截**：LLM 嘗試寫入但跳過了前置條件 → `Blocked` guard outcome
   - **Happy path 確認**：使用者意圖與 LLM 執行路徑完全吻合 → 值得作為正向範例
   - **Performance 異常**：簡單請求但 `round_count` 異常高 → `RoundCountLe` budget case
5. 針對每個有價值的模式，呼叫 `propose_eval_case` 儲存提案
6. 提案後立即呼叫 `run_eval_case` 驗證你的提案是否能正確抓到該模式（pass = 案例設計正確）

## Trace 欄位說明

每個 trace 包含以下欄位，請善用它們輔助判斷：
- `blocked_calls`：被 guard 攔截的工具呼叫數
- `round_count`：LLM 回合數（高數值可能代表效率問題）
- `skill_activations`：本次對話觸發的技能名稱清單
- `memory_facts_injected`：注入 context 的記憶片段數量
  - 值為 0 代表記憶沒有命中，表示這是全新話題或記憶系統未命中
  - 值 > 0 代表有記憶輔助，可評估是否影響了工具選擇

## 判斷標準

**值得提案的情況：**
- trace 顯示某工具被 guard blocked，且對話中使用者的意圖確實需要先讀取才能寫入
- 一個完整的讀→寫序列在對話中被成功執行，可作為 happy path 範本
- `blocked_calls > 0` 且對話顯示 LLM 在第二輪自行修正了路徑（說明 guard 有效）
- `memory_facts_injected = 0` 且 `round_count` 異常高（記憶缺失導致效率下降）

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
- 提案後一定要呼叫 `run_eval_case` 自我驗證；若 fail，呼叫 `update_proposed_eval_case` 修正 tool_sequence 或 assertions，再重新 `run_eval_case`，直到 pass 為止
"#;
