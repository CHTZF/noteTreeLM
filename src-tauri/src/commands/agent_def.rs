use crate::{db::surreal::SurrealDb, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

// ── Data Structures ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AgentDefinition {
    pub def_id: String,
    pub vault_id: String,
    pub name: String,
    pub description: String,
    pub kind: String,           // 'main' | 'sub'
    pub skill_ids: Vec<String>,
    pub tool_names: Vec<String>,
    pub system_prompt: String,
    pub max_rounds: i64,
    pub is_active: bool,
    pub is_builtin: bool,
    pub trigger: String,
    pub created_at: i64,        // ms timestamp
    pub status: String,         // 'active' | 'sleep'
    pub slept_at: Option<i64>,  // ms timestamp, set when entering sleep
    pub use_count: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Deserialize)]
struct DefRow {
    def_id: String,
    vault_id: String,
    name: String,
    description: String,
    kind: String,
    skill_ids: Vec<String>,
    tool_names: Vec<String>,
    system_prompt: String,
    max_rounds: i64,
    is_active: bool,
    is_builtin: bool,
    #[serde(default)]
    trigger: String,
    created_at: surrealdb::sql::Datetime,
    #[serde(default)]
    status: String,
    slept_at: Option<surrealdb::sql::Datetime>,
    #[serde(default)]
    use_count: i64,
    last_used_at: Option<surrealdb::sql::Datetime>,
}

impl From<DefRow> for AgentDefinition {
    fn from(r: DefRow) -> Self {
        AgentDefinition {
            def_id: r.def_id,
            vault_id: r.vault_id,
            name: r.name,
            description: r.description,
            kind: r.kind,
            skill_ids: r.skill_ids,
            tool_names: r.tool_names,
            system_prompt: r.system_prompt,
            max_rounds: r.max_rounds,
            is_active: r.is_active,
            is_builtin: r.is_builtin,
            trigger: r.trigger,
            created_at: r.created_at.timestamp_millis(),
            status: if r.status.is_empty() { "active".to_string() } else { r.status },
            slept_at: r.slept_at.map(|d| d.timestamp_millis()),
            use_count: r.use_count,
            last_used_at: r.last_used_at.map(|d| d.timestamp_millis()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn valid_kind(s: &str) -> &str {
    match s { "main" | "sub" => s, _ => "sub" }
}

/// Compute trigger embedding via llama-server (always available when chat is running)
async fn compute_trigger_embedding(
    trigger: &str,
    state: &AppState,
) -> Option<Vec<f32>> {
    if trigger.trim().is_empty() {
        return None;
    }
    let port = *state.llama_actual_port.lock().await;
    let llama_url = port.map(|p| format!("http://127.0.0.1:{}", p))?;
    let client = reqwest::Client::new();
    let vec = crate::commands::ai::get_embedding(&client, &llama_url, trigger).await;
    if vec.is_empty() { None } else { Some(vec) }
}

// ── Commands ──────────────────────────────────────────────────────────────────

/// 列出 vault 所有 agent definitions（builtin + 使用者自訂）
#[tauri::command]
pub async fn list_agent_definitions(
    state: State<'_, AppState>,
) -> Result<Vec<AgentDefinition>, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;

    let mut resp = db.query(
        "SELECT def_id, vault_id, name, description, kind, skill_ids, tool_names, \
                system_prompt, max_rounds, is_active, is_builtin, trigger OR '' AS trigger, created_at, \
                status OR 'active' AS status, slept_at, use_count OR 0 AS use_count, last_used_at \
         FROM agent_definitions \
         WHERE vault_id = $vid \
         ORDER BY is_builtin DESC, created_at ASC"
    )
    .bind(("vid", vault_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    let rows: Vec<DefRow> = resp.take(0).unwrap_or_else(|e| {
        eprintln!("[list_agent_definitions] deserialize error: {e}");
        vec![]
    });

    Ok(rows.into_iter().map(AgentDefinition::from).collect())
}

/// 建立使用者自訂的 agent definition
#[tauri::command]
pub async fn save_agent_definition(
    state: State<'_, AppState>,
    name: String,
    description: String,
    kind: Option<String>,
    skill_ids: Vec<String>,
    tool_names: Vec<String>,
    system_prompt: Option<String>,
    max_rounds: Option<i64>,
    trigger: Option<String>,
) -> Result<AgentDefinition, AppError> {
    let vault_id = state.get_vault_id().await?;
    let db = &state.db;
    let def_id = uuid::Uuid::new_v4().to_string();
    let kind_str = valid_kind(kind.as_deref().unwrap_or("sub")).to_string();
    let prompt = system_prompt.unwrap_or_default();
    let rounds = max_rounds.unwrap_or(5).max(1).min(20);
    let trigger_str = trigger.unwrap_or_default();

    let trigger_embedding = compute_trigger_embedding(&trigger_str, state.inner()).await;

    db.query(
        "INSERT INTO agent_definitions \
         (def_id, vault_id, name, description, kind, skill_ids, tool_names, \
          system_prompt, max_rounds, is_active, is_builtin, trigger, trigger_embedding, created_at, \
          status, use_count, last_used_at, slept_at) \
         VALUES ($did, $vid, $name, $desc, $kind, $skills, $tools, \
                 $prompt, $rounds, true, false, $trigger, $temb, time::now(), \
                 'active', 0, NONE, NONE)"
    )
    .bind(("did",    def_id.clone()))
    .bind(("vid",    vault_id.clone()))
    .bind(("name",   name.clone()))
    .bind(("desc",   description.clone()))
    .bind(("kind",   kind_str.clone()))
    .bind(("skills", skill_ids.clone()))
    .bind(("tools",  tool_names.clone()))
    .bind(("prompt", prompt.clone()))
    .bind(("rounds", rounds))
    .bind(("trigger", trigger_str.clone()))
    .bind(("temb",   trigger_embedding))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    Ok(AgentDefinition {
        def_id,
        vault_id,
        name,
        description,
        kind: kind_str,
        skill_ids,
        tool_names,
        system_prompt: prompt,
        max_rounds: rounds,
        is_active: true,
        is_builtin: false,
        trigger: trigger_str,
        created_at: chrono::Utc::now().timestamp_millis(),
        status: "active".to_string(),
        slept_at: None,
        use_count: 0,
        last_used_at: None,
    })
}

/// 更新 agent definition（builtin 也可更新 skill_ids/tool_names/system_prompt/max_rounds/trigger）
#[tauri::command]
pub async fn update_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
    name: String,
    description: String,
    skill_ids: Vec<String>,
    tool_names: Vec<String>,
    system_prompt: Option<String>,
    max_rounds: Option<i64>,
    trigger: Option<String>,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;
    let prompt = system_prompt.unwrap_or_default();
    let rounds = max_rounds.unwrap_or(5).max(1).min(20);
    let trigger_str = trigger.unwrap_or_default();

    let trigger_embedding = compute_trigger_embedding(&trigger_str, state.inner()).await;

    state.db.query(
        "UPDATE agent_definitions SET \
           name = $name, description = $desc, skill_ids = $skills, \
           tool_names = $tools, system_prompt = $prompt, max_rounds = $rounds, \
           trigger = $trigger, trigger_embedding = $temb \
         WHERE vault_id = $vid AND def_id = $did"
    )
    .bind(("name",   name))
    .bind(("desc",   description))
    .bind(("skills", skill_ids))
    .bind(("tools",  tool_names))
    .bind(("prompt", prompt))
    .bind(("rounds", rounds))
    .bind(("trigger", trigger_str))
    .bind(("temb",   trigger_embedding))
    .bind(("vid",    vault_id))
    .bind(("did",    def_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// 刪除使用者自訂的 agent definition（is_builtin = true 的不能刪）
#[tauri::command]
pub async fn delete_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;

    state.db.query(
        "DELETE agent_definitions \
         WHERE vault_id = $vid AND def_id = $did AND is_builtin = false"
    )
    .bind(("vid", vault_id))
    .bind(("did", def_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// 啟用或停用一個 agent definition
#[tauri::command]
pub async fn toggle_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
    is_active: bool,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;

    state.db.query(
        "UPDATE agent_definitions SET is_active = $active \
         WHERE vault_id = $vid AND def_id = $did"
    )
    .bind(("active", is_active))
    .bind(("vid",    vault_id))
    .bind(("did",    def_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// 列出本次 session 中所有 ephemeral agents（供 AgentsPage UI 顯示）
#[tauri::command]
pub async fn list_ephemeral_agents(
    state: State<'_, AppState>,
) -> Result<Vec<crate::runtime::system_agent::EphemeralAgent>, AppError> {
    Ok(state.system_agent.list_ephemeral().await)
}

/// 清除本次 session 的 ephemeral agents 記錄
#[tauri::command]
pub async fn clear_ephemeral_agents(
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    state.system_agent.clear_session().await;
    Ok(())
}

/// 從 DB 取得單一 definition（供 SystemAgentService 路由用）
pub async fn get_agent_definition_by_id(
    db: &SurrealDb,
    vault_id: &str,
    def_id: &str,
) -> Option<AgentDefinition> {
    let mut resp = db.query(
        "SELECT def_id, vault_id, name, description, kind, skill_ids, tool_names, \
                system_prompt, max_rounds, is_active, is_builtin, trigger OR '' AS trigger, created_at, \
                status OR 'active' AS status, slept_at, use_count OR 0 AS use_count, last_used_at \
         FROM agent_definitions \
         WHERE vault_id = $vid AND def_id = $did AND is_active = true \
         LIMIT 1"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("did", def_id.to_string()))
    .await.ok()?;

    let rows: Vec<DefRow> = resp.take(0).ok()?;
    rows.into_iter().next().map(AgentDefinition::from)
}

/// 按 name（模糊）或 kind 找最符合的 definition（供 SystemAgentService 路由用）
pub async fn find_agent_definition(
    db: &SurrealDb,
    vault_id: &str,
    target: &str,
) -> Option<AgentDefinition> {
    // 先嘗試 def_id 精確匹配
    if let Some(def) = get_agent_definition_by_id(db, vault_id, target).await {
        return Some(def);
    }

    // fallback：雙向 contains（name↔target）或 description CONTAINS target
    // 包含 sleep 狀態的 agent（route() 命中後會自動 wake）
    let mut resp = db.query(
        "SELECT def_id, vault_id, name, description, kind, skill_ids, tool_names, \
                system_prompt, max_rounds, is_active, is_builtin, trigger OR '' AS trigger, created_at, \
                status OR 'active' AS status, slept_at, use_count OR 0 AS use_count, last_used_at \
         FROM agent_definitions \
         WHERE vault_id = $vid AND is_active = true \
           AND (string::lowercase(name) CONTAINS string::lowercase($target) \
             OR string::lowercase($target) CONTAINS string::lowercase(name) \
             OR string::lowercase(description) CONTAINS string::lowercase($target)) \
         ORDER BY is_builtin DESC \
         LIMIT 1"
    )
    .bind(("vid",    vault_id.to_string()))
    .bind(("target", target.to_string()))
    .await.ok()?;

    let rows: Vec<DefRow> = resp.take(0).ok()?;
    rows.into_iter().next().map(AgentDefinition::from)
}

/// 找到與 user_embedding cosine similarity 最高的 agent definition
/// active_only=true：只匹配 status='active'（pre-routing 用）
/// active_only=false：也匹配 sleep agent（route() fallback 用，命中後由呼叫方 wake）
pub async fn find_matching_agent_definition(
    db: &SurrealDb,
    vault_id: &str,
    user_embedding: &[f32],
    threshold: f32,
    active_only: bool,
) -> Option<AgentDefinition> {
    #[derive(Deserialize)]
    struct EmbRow {
        def_id: String,
        vault_id: String,
        name: String,
        description: String,
        kind: String,
        skill_ids: Vec<String>,
        tool_names: Vec<String>,
        system_prompt: String,
        max_rounds: i64,
        is_active: bool,
        is_builtin: bool,
        #[serde(default)]
        trigger: String,
        created_at: surrealdb::sql::Datetime,
        trigger_embedding: Vec<f32>,
        #[serde(default)]
        status: String,
        slept_at: Option<surrealdb::sql::Datetime>,
        #[serde(default)]
        use_count: i64,
        last_used_at: Option<surrealdb::sql::Datetime>,
    }

    let status_filter = if active_only {
        "AND (status = 'active' OR status = NONE)"
    } else {
        ""
    };

    let query = format!(
        "SELECT def_id, vault_id, name, description, kind, skill_ids, tool_names, \
                system_prompt, max_rounds, is_active, is_builtin, trigger OR '' AS trigger, \
                created_at, trigger_embedding, \
                status OR 'active' AS status, slept_at, use_count OR 0 AS use_count, last_used_at \
         FROM agent_definitions \
         WHERE vault_id = $vid AND is_active = true AND trigger_embedding != NONE {}",
        status_filter
    );

    let mut resp = db.query(query)
        .bind(("vid", vault_id.to_string()))
        .await.ok()?;

    let rows: Vec<EmbRow> = resp.take(0).ok()?;
    if rows.is_empty() {
        return None;
    }

    let mut best_score = threshold;
    let mut best: Option<EmbRow> = None;

    for row in rows {
        let score = cosine_similarity(user_embedding, &row.trigger_embedding);
        if score > best_score {
            best_score = score;
            best = Some(row);
        }
    }

    best.map(|r| AgentDefinition {
        def_id: r.def_id,
        vault_id: r.vault_id,
        name: r.name,
        description: r.description,
        kind: r.kind,
        skill_ids: r.skill_ids,
        tool_names: r.tool_names,
        system_prompt: r.system_prompt,
        max_rounds: r.max_rounds,
        is_active: r.is_active,
        is_builtin: r.is_builtin,
        trigger: r.trigger,
        created_at: r.created_at.timestamp_millis(),
        status: if r.status.is_empty() { "active".to_string() } else { r.status },
        slept_at: r.slept_at.map(|d| d.timestamp_millis()),
        use_count: r.use_count,
        last_used_at: r.last_used_at.map(|d| d.timestamp_millis()),
    })
}

/// 記錄 agent 被呼叫（use_count+1, last_used_at=now, 若在 sleep 則自動 wake）
pub async fn record_agent_usage(db: &SurrealDb, vault_id: &str, def_id: &str) {
    let _ = db.query(
        "UPDATE agent_definitions SET \
           use_count = (use_count OR 0) + 1, \
           last_used_at = time::now(), \
           status = 'active', \
           slept_at = NONE \
         WHERE vault_id = $vid AND def_id = $did"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("did", def_id.to_string()))
    .await;
}

/// 生命週期管理：進 sleep（7天）、刪除（sleep 23天）
/// 在 app 啟動及每次 invoke_agent 時呼叫
pub async fn check_agent_lifecycle(db: &SurrealDb, vault_id: &str) {
    let now = chrono::Utc::now();
    let sleep_cutoff = now - chrono::TimeDelta::days(7);
    let delete_cutoff = now - chrono::TimeDelta::days(23);

    // 1. 刪除 sleep 超過 23 天的 agent
    let _ = db.query(
        "DELETE agent_definitions \
         WHERE vault_id = $vid AND is_builtin = false \
           AND status = 'sleep' AND slept_at != NONE AND slept_at < $dcutoff"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("dcutoff", surrealdb::sql::Datetime::from(delete_cutoff)))
    .await;

    // 2. 將 7 天未使用的 active agent 設為 sleep
    let _ = db.query(
        "UPDATE agent_definitions SET status = 'sleep', slept_at = time::now() \
         WHERE vault_id = $vid AND is_builtin = false AND status != 'sleep' \
           AND ( (last_used_at != NONE AND last_used_at < $scutoff) \
              OR (last_used_at = NONE  AND created_at  < $scutoff) )"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("scutoff", surrealdb::sql::Datetime::from(sleep_cutoff)))
    .await;
}

/// 手動 wake 一個 sleep 中的 agent（前端 UI 呼叫）
#[tauri::command]
pub async fn wake_agent_definition(
    state: State<'_, AppState>,
    def_id: String,
) -> Result<(), AppError> {
    let vault_id = state.get_vault_id().await?;

    state.db.query(
        "UPDATE agent_definitions SET status = 'active', slept_at = NONE \
         WHERE vault_id = $vid AND def_id = $did AND is_builtin = false"
    )
    .bind(("vid", vault_id))
    .bind(("did", def_id))
    .await.map_err(|e| AppError::Database(e.to_string()))?;

    Ok(())
}

/// 種子內建 agent definitions（每次啟動幂等重建）。
/// 包含：技能建立助理、排程助理、筆記整理助理。
pub async fn seed_agent_definitions(db: &SurrealDb, vault_id: &str, emb_url: Option<&str>) {
    if db.use_ns("notetreelm").use_db("main").await.is_err() { return; }

    // 從 vault_tools() 提取所有工具清單（供技能建立助理 system_prompt 使用）
    let tools_desc: String = crate::commands::ai::vault_tools()
        .as_array()
        .map(|arr| {
            arr.iter().filter_map(|t| {
                let f = t.get("function")?;
                let name = f.get("name")?.as_str()?;
                let desc = f.get("description")?.as_str().unwrap_or("");
                Some(format!("- **{}**: {}", name, desc.lines().next().unwrap_or(desc)))
            }).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default();

    struct BuiltinAgent {
        id:         &'static str,
        name:       &'static str,
        description:&'static str,
        kind:       &'static str,
        tool_names: Vec<String>,
        system_prompt: String,
        max_rounds: i64,
        trigger:    &'static str,
    }

    let agents: Vec<BuiltinAgent> = vec![
        BuiltinAgent {
            id:          "builtin_skill_builder",
            name:        "技能建立助理",
            description: "根據知識描述自動設計並建立 Agent 技能規範",
            kind:        "sub",
            tool_names:  vec!["create_agent_skill".to_string()],
            system_prompt: format!(
                "你是技能建立助理，專門根據描述設計 Agent 技能規範。\n\
                 \n\
                 ## 系統可用工具（tool_calls 只能從此清單選擇）\n\
                 {tools_desc}\n\
                 \n\
                 ## 任務\n\
                 根據使用者提供的知識描述或需求，呼叫 create_agent_skill 設計 1-2 個技能規範。\n\
                 每個技能必須有清楚的觸發情境（trigger）、分步驟的行為描述（behavior）、\n\
                 以及從上方清單中選擇真正需要的工具（tool_calls）。\n\
                 只呼叫 create_agent_skill 工具，不要輸出其他文字。"
            ),
            max_rounds:  3,
            trigger:     "建立技能規範、新增技能、幫我設計skill、創建技能、新建skill、design skill、create skill spec",
        },
        BuiltinAgent {
            id:          "builtin_scheduler",
            name:        "排程助理",
            description: "幫使用者設定任務提醒、定時通知與週期性排程",
            kind:        "sub",
            tool_names:  vec!["get_current_datetime".to_string(), "schedule_task".to_string()],
            system_prompt: "你是排程助理，專門幫使用者設定任務提醒與排程。\n\
                 \n\
                 ## 工作流程\n\
                 1. 先呼叫 get_current_datetime 確認現在的時間與時區。\n\
                 2. 根據使用者描述計算執行時間 run_at（ISO 8601 含時區，如 2026-03-22T09:00:00+08:00）。\n\
                 3. 若需重複，填 repeat_interval_seconds（每天=86400、每週=604800、每月≈2592000，不重複填 0）。\n\
                 4. 呼叫 schedule_task（description、run_at、repeat_interval_seconds）完成排程。\n\
                 5. 用友善語氣確認排程結果（告知使用者會在何時收到提醒）。\n\
                 \n\
                 ## 注意\n\
                 - 時間若使用者只說「明天」、「三點」等相對語，需結合步驟1的現在時間計算。\n\
                 - 永遠確保 run_at 是未來時間。"
                 .to_string(),
            max_rounds:  3,
            trigger:     "排程、設定提醒、定時任務、設定鬧鐘、幫我排程、提醒我、到時候提醒、schedule、remind me、set reminder、每天提醒、每週提醒、固定時間、重複執行",
        },
        BuiltinAgent {
            id:          "builtin_note_card_advisor",
            name:        "筆記卡片助理",
            description: "根據知識內容分析並建立 concept/procedure/reference 型筆記卡片",
            kind:        "sub",
            tool_names:  vec![
                "search_vault".to_string(), "read_note".to_string(),
                "create_note".to_string(), "plan_announce".to_string(),
            ],
            system_prompt: "你是筆記卡片助理，專門根據知識內容生成結構化的筆記卡片。\n\
                 \n\
                 ## 卡片模板類型\n\
                 - **concept**：概念定義卡，包含定義、詳細說明、範例\n\
                 - **procedure**：操作步驟卡，包含前提條件、步驟清單、注意事項\n\
                 - **reference**：參考資料卡，包含摘要、重要連結、關鍵點清單\n\
                 \n\
                 ## 每張卡片的 Markdown 格式\n\
                 ```\n\
                 ---\n\
                 status: draft\n\
                 tags: [concept]\n\
                 ---\n\
                 \n\
                 # 標題\n\
                 \n\
                 ## 定義 / 步驟 / 摘要\n\
                 （核心內容）\n\
                 ```\n\
                 \n\
                 ## 工作流程\n\
                 1. 若使用者指定筆記，呼叫 search_vault 或 read_note 取得原始內容。\n\
                 2. 分析內容，決定適合哪些模板類型（通常 2-3 張）。\n\
                 3. 呼叫 plan_announce 列出將建立的卡片清單，deferred_tools 填 create_note。\n\
                 4. 使用者確認後，逐一呼叫 create_note（路徑格式：cards/[標題].md）。\n\
                 5. 回覆使用者已建立的卡片列表。"
                 .to_string(),
            max_rounds:  5,
            trigger:     "建立筆記卡片、知識卡片、建議卡片、整理成卡片、幫我做成卡片、suggest note card、create note card、concept 卡片、procedure 卡片、reference 卡片",
        },
        BuiltinAgent {
            id:          "builtin_note_summarizer",
            name:        "筆記整理助理",
            description: "閱讀多篇筆記並產出摘要、彙整或結構化整理",
            kind:        "sub",
            tool_names:  vec![
                "search_vault".to_string(), "list_notes_in_folder".to_string(),
                "list_structure".to_string(), "read_note".to_string(),
                "create_note".to_string(), "update_note".to_string(),
                "plan_announce".to_string(),
            ],
            system_prompt: "你是筆記整理助理，專門閱讀多篇筆記並產出摘要或結構化整理。\n\
                 \n\
                 ## 工作流程\n\
                 1. 用 search_vault 或 list_notes_in_folder 取得相關筆記清單。\n\
                 2. 對清單中每篇筆記呼叫 read_note 讀取完整內容。\n\
                 3. 彙整後輸出結構化摘要（分節標題、要點、結論）。\n\
                 4. 若使用者希望儲存整理成果，呼叫 plan_announce 說明計畫，\n\
                    使用者確認後再 create_note 或 update_note 寫入。\n\
                 \n\
                 ## 注意\n\
                 - 摘要要忠實反映筆記原意，不要自行發明內容。\n\
                 - 若筆記數量超過 10 篇，先列出清單讓使用者確認範圍再逐一閱讀。"
                 .to_string(),
            max_rounds:  8,
            trigger:     "整理筆記、摘要多篇、幫我歸納、彙整資料、總結所有、整合多篇、summarize notes、consolidate、把這些筆記整理、歸納重點、統整一下、彙整成一篇、做個摘要、整理一份報告",
        },
    ];

    // 每次啟動重建（確保 system_prompt / tool_names 始終最新）
    let _ = db.query(
        "DELETE agent_definitions WHERE vault_id = $vid AND is_builtin = true"
    )
    .bind(("vid", vault_id.to_string()))
    .await;

    let client = reqwest::Client::new();
    let skill_ids: Vec<String> = vec![];

    for agent in &agents {
        let trigger_embedding: Option<Vec<f32>> = if let Some(url) = emb_url {
            let emb = crate::commands::ai::get_embedding(&client, url, agent.trigger).await;
            if emb.is_empty() { None } else { Some(emb) }
        } else {
            None
        };

        let _ = db.query(
            "INSERT INTO agent_definitions \
             (def_id, vault_id, name, description, kind, skill_ids, tool_names, \
              system_prompt, max_rounds, is_active, is_builtin, trigger, trigger_embedding, created_at, \
              status, use_count, last_used_at, slept_at) \
             VALUES ($did, $vid, $name, $desc, $kind, $skills, $tools, \
                     $prompt, $rounds, true, true, $trigger, $temb, time::now(), \
                     'active', 0, NONE, NONE)"
        )
        .bind(("did",     agent.id.to_string()))
        .bind(("vid",     vault_id.to_string()))
        .bind(("name",    agent.name.to_string()))
        .bind(("desc",    agent.description.to_string()))
        .bind(("kind",    agent.kind.to_string()))
        .bind(("skills",  skill_ids.clone()))
        .bind(("tools",   agent.tool_names.clone()))
        .bind(("prompt",  agent.system_prompt.clone()))
        .bind(("rounds",  agent.max_rounds))
        .bind(("trigger", agent.trigger.to_string()))
        .bind(("temb",    trigger_embedding))
        .await;
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { 0.0 } else { dot / (norm_a * norm_b) }
}
