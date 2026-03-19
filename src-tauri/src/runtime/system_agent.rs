// runtime/system_agent.rs
//
// SystemAgentService — rule-based broker，不使用 LLM。
//
// 職責：
// 1. 維護 agent_definitions 快取（從 DB 載入，支援熱重載）
// 2. 路由 call_agent 請求到對應 definition
// 3. 執行 sub-agent 並同步回傳結果
// 4. 記錄所有請求到 debug event stream
// 5. 追蹤本次 session 的 ephemeral agents

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::commands::agent_def::find_agent_definition;
use crate::db::surreal::SurrealDb;
use crate::runtime::tool_registry::ToolRegistry;
use crate::runtime::types::{EmitEventFn, LlmFn};

// ── NewSkillSpec（create_agent 工具傳入的 skill 規格）────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct NewSkillSpec {
    pub title: String,
    pub trigger: String,
    pub behavior: String,
    #[serde(default = "default_passive")]
    pub injection_mode: String,
}

fn default_passive() -> String { "passive".to_string() }

// ── Ephemeral agent record (本次 session 中動態建立的 agent) ──────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EphemeralAgent {
    pub sub_session_id: String,
    pub def_id: String,          // 使用的 definition（可能是 builtin 或自訂）
    pub def_name: String,
    pub task: String,
    pub status: String,          // "running" | "done" | "error"
    pub result_preview: String,  // 前 200 字
    pub started_at: i64,         // ms timestamp
    pub ended_at: Option<i64>,
}

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct AgentRequest {
    pub caller_session_id: String,
    pub target: String,   // def_id 或 name（模糊匹配）
    pub task: String,
    pub context: String,
}

// ── SystemAgentService ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SystemAgentService {
    db: SurrealDb,
    vault_id: Arc<RwLock<String>>,
    /// 本次 session 的 ephemeral agents（供 UI 顯示）
    pub ephemeral: Arc<RwLock<Vec<EphemeralAgent>>>,
}

impl SystemAgentService {
    pub fn new(db: SurrealDb) -> Self {
        Self {
            db,
            vault_id: Arc::new(RwLock::new(String::new())),
            ephemeral: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn set_vault_id(&self, vid: String) {
        *self.vault_id.write().await = vid;
    }

    /// 清空本 session 的 ephemeral 記錄（每次 Chat session 開始前呼叫）
    pub async fn clear_session(&self) {
        self.ephemeral.write().await.clear();
    }

    /// 取得目前 ephemeral agents 清單（供前端 list_ephemeral_agents 呼叫）
    pub async fn list_ephemeral(&self) -> Vec<EphemeralAgent> {
        self.ephemeral.read().await.clone()
    }

    /// 路由並同步執行一個 agent 任務，回傳最終文字結果。
    pub async fn route(
        &self,
        request: AgentRequest,
        tools_json: Value,
        registry: Arc<ToolRegistry>,
        llm_fn: LlmFn,
        emit: EmitEventFn,
        cancel: Option<Arc<AtomicBool>>,
        emb_url: Option<&str>,
    ) -> String {
        let vault_id = self.vault_id.read().await.clone();

        // ── 1. 找 definition（name 模糊匹配 → embedding fallback，含 sleep agent）──
        let def = {
            let by_name = find_agent_definition(&self.db, &vault_id, &request.target).await;
            if by_name.is_some() {
                by_name
            } else if let Some(url) = emb_url {
                let client = reqwest::Client::new();
                let target_emb = crate::commands::ai::get_embedding(
                    &client, url, &request.target).await;
                if !target_emb.is_empty() {
                    crate::commands::agent_def::find_matching_agent_definition(
                        &self.db, &vault_id, &target_emb, 0.55, false,
                    ).await
                } else { None }
            } else { None }
        };

        let (def_id, def_name, max_rounds, skill_ids, agent_tool_names, system_prompt_override) =
            if let Some(ref d) = def {
                (
                    d.def_id.clone(),
                    d.name.clone(),
                    d.max_rounds as usize,
                    d.skill_ids.clone(),
                    d.tool_names.clone(),
                    d.system_prompt.clone(),
                )
            } else {
                // Fallback：找不到 definition，使用 target 名稱建立臨時設定
                eprintln!("[SystemAgent] 找不到 definition '{}'，使用 fallback", request.target);
                emit(
                    "system_agent:warn".into(),
                    serde_json::json!({
                        "caller": request.caller_session_id,
                        "target": request.target,
                        "warn": "找不到對應 agent definition，使用預設設定",
                    }),
                );
                (
                    format!("ephemeral-{}", request.target),
                    request.target.clone(),
                    5,
                    vec![],
                    vec![],
                    String::new(),
                )
            };

        // ── 2. 過濾工具（依 definition 的 tool_names，始終排除 call_agent 避免遞迴）─
        let filtered_tools: Value = {
            let arr = tools_json.as_array().cloned().unwrap_or_default();
            Value::Array(
                arr.into_iter()
                    .filter(|t| {
                        let name = t["function"]["name"].as_str().unwrap_or("");
                        if name == "call_agent" || name == "touch_agent" { return false; }
                        if agent_tool_names.is_empty() {
                            true
                        } else {
                            agent_tool_names.iter().any(|n| n == name)
                        }
                    })
                    .collect(),
            )
        };

        // ── 3. 動態 skill 注入：embedding 比對當前 task，補齊靜態 skill_ids ──────
        //    只有 agent_scope IN ('all','sub') 且 is_active=true 的 skills 參與
        let mut skill_ids = skill_ids;
        if let Some(url) = emb_url {
            let client = reqwest::Client::new();
            let task_emb = crate::commands::ai::get_embedding(&client, url, &request.task).await;
            if !task_emb.is_empty() {
                #[derive(serde::Deserialize)]
                struct SkillEmbRow { skill_id: String, trigger_embedding: Vec<f32> }
                let mut resp = self.db.query(
                    "SELECT skill_id, trigger_embedding FROM agent_skills \
                     WHERE vault_id = $vid AND is_active = true \
                       AND (agent_scope = 'all' OR agent_scope = 'sub') \
                       AND trigger_embedding != NONE"
                )
                .bind(("vid", vault_id.clone()))
                .await.ok();
                if let Some(ref mut r) = resp {
                    if let Ok(rows) = r.take::<Vec<SkillEmbRow>>(0) {
                        for row in rows {
                            if skill_ids.contains(&row.skill_id) { continue; }
                            let sim = crate::commands::agent_def::cosine_similarity(
                                &task_emb, &row.trigger_embedding);
                            if sim >= 0.60 {
                                skill_ids.push(row.skill_id);
                            }
                        }
                    }
                }
            }
        }

        // ── 3b. 組建 system prompt（加入 skills 注入 + override）─────────────
        let skill_section = if !skill_ids.is_empty() {
            self.build_skill_section(&skill_ids, &vault_id).await
        } else {
            String::new()
        };

        let base_system = if system_prompt_override.is_empty() {
            format!(
                "你是一個專業的 {def_name} 助理，負責完成指定子任務。\
                 請簡潔地完成任務，回傳結果摘要，不要加入多餘的說明。"
            )
        } else {
            system_prompt_override
        };

        let full_system = if skill_section.is_empty() {
            base_system
        } else {
            format!("{base_system}\n\n{skill_section}")
        };

        // ── 4. 記錄 ephemeral agent ───────────────────────────────────────────
        let sub_session_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().timestamp_millis();

        {
            let mut eph = self.ephemeral.write().await;
            eph.push(EphemeralAgent {
                sub_session_id: sub_session_id.clone(),
                def_id: def_id.clone(),
                def_name: def_name.clone(),
                task: request.task.clone(),
                status: "running".to_string(),
                result_preview: String::new(),
                started_at,
                ended_at: None,
            });
        }

        // ── 5. Emit route event ───────────────────────────────────────────────
        emit(
            "system_agent:route".into(),
            serde_json::json!({
                "caller_session_id": request.caller_session_id,
                "sub_session_id": sub_session_id,
                "def_id": def_id,
                "def_name": def_name,
                "task": request.task,
            }),
        );

        // ── 6. 執行 sub-agent ─────────────────────────────────────────────────
        let result = run_sub_agent_with_system(
            sub_session_id.clone(),
            request.caller_session_id,
            &def_name,
            &request.task,
            &full_system,
            filtered_tools,
            registry,
            llm_fn,
            emit.clone(),
            max_rounds,
            cancel,
        )
        .await;

        // ── 7. 更新 ephemeral 狀態 ────────────────────────────────────────────
        {
            let mut eph = self.ephemeral.write().await;
            if let Some(e) = eph.iter_mut().find(|e| e.sub_session_id == sub_session_id) {
                e.status = if result.starts_with("[sub-agent 錯誤]") || result.starts_with("[sub-agent tool error]") {
                    "error".to_string()
                } else {
                    "done".to_string()
                };
                e.result_preview = result.chars().take(200).collect();
                e.ended_at = Some(chrono::Utc::now().timestamp_millis());
            }
        }

        // 記錄使用（use_count+1, last_used_at=now, 若 sleep 自動 wake）
        // 只對有 DB definition 的 agent 記錄（ephemeral fallback 的 def_id 以 "ephemeral-" 開頭）
        if !def_id.starts_with("ephemeral-") {
            crate::commands::agent_def::record_agent_usage(&self.db, &vault_id, &def_id).await;
        }

        // Emit updated ephemeral list to frontend
        emit(
            "system_agent:ephemeral_updated".into(),
            serde_json::json!({
                "sub_session_id": sub_session_id,
                "def_name": def_name,
                "status": "done",
            }),
        );

        result
    }

    /// 動態建立 Agent Definition，自動 find-or-create 所需的 skills（含 embedding）。
    /// 用 user_ask 的 embedding 搜尋語意相似的現有 agent（閾值 0.75）；
    /// 找到則直接複用，找不到則以傳入的參數 create 新 agent。
    pub async fn touch_agent(
        &self,
        name: String,
        description: String,
        trigger: String,
        tool_names: Vec<String>,
        skills: Vec<NewSkillSpec>,
        max_rounds: i64,
        emb_url: Option<&str>,
        emit: EmitEventFn,
        user_ask: &str,
    ) -> Result<crate::commands::agent_def::AgentDefinition, String> {
        let vault_id = self.vault_id.read().await.clone();
        if vault_id.is_empty() {
            return Err("Vault 未設定".to_string());
        }

        // 1. 用 user_ask embedding 比對現有 agent（閾值 0.75，包含 sleep agent）
        if let Some(url) = emb_url {
            let client = reqwest::Client::new();
            let ask_emb = crate::commands::ai::get_embedding(&client, url, user_ask).await;
            if !ask_emb.is_empty() {
                if let Some(existing) = crate::commands::agent_def::find_matching_agent_definition(
                    &self.db, &vault_id, &ask_emb, 0.75, false,
                ).await {
                    return Ok(existing);
                }
            }
        }

        // 2. 找不到相似 agent → 建立新的
        self.create_agent(name, description, trigger, tool_names, skills, max_rounds, emb_url, emit).await
    }

    /// 由 `create_agent` 工具呼叫，Chat LLM 透過此方法有機地建立專用 agent。
    pub async fn create_agent(
        &self,
        name: String,
        description: String,
        trigger: String,
        tool_names: Vec<String>,
        skills: Vec<NewSkillSpec>,
        max_rounds: i64,
        emb_url: Option<&str>,
        emit: EmitEventFn,
    ) -> Result<crate::commands::agent_def::AgentDefinition, String> {
        let vault_id = self.vault_id.read().await.clone();
        if vault_id.is_empty() {
            return Err("Vault 未設定，無法建立 Agent".to_string());
        }
        let client = reqwest::Client::new();

        // ── 0a. 計算 trigger embedding（後續 dedup 用）─────────────────
        let trigger_emb_for_dedup: Vec<f32> = if let Some(url) = emb_url {
            if !trigger.trim().is_empty() {
                crate::commands::ai::get_embedding(&client, url, &trigger).await
            } else { vec![] }
        } else { vec![] };

        // ── 0b. Embedding dedup：語意相似度 > 0.85 → 複用現有 agent ──
        if !trigger_emb_for_dedup.is_empty() {
            if let Some(existing) = crate::commands::agent_def::find_matching_agent_definition(
                &self.db, &vault_id, &trigger_emb_for_dedup, 0.85, false,
            ).await {
                emit(
                    "system_agent:agent_created".into(),
                    serde_json::json!({ "def_id": existing.def_id, "name": existing.name, "reused": true }),
                );
                return Ok(existing);
            }
        }

        // ── 0c. 同名 dedup：直接複用，不報錯 ────────────────────────
        if let Some(existing) = crate::commands::agent_def::find_agent_definition(
            &self.db, &vault_id, &name,
        ).await {
            emit(
                "system_agent:agent_created".into(),
                serde_json::json!({ "def_id": existing.def_id, "name": existing.name, "reused": true }),
            );
            return Ok(existing);
        }

        // ── 1. find-or-create 每個 skill ─────────────────────────────
        let mut skill_ids: Vec<String> = Vec::new();
        for spec in &skills {
            let sid = self
                .find_or_create_skill(spec, &vault_id, emb_url, &client)
                .await
                .map_err(|e| format!("skill '{}' 建立失敗: {}", spec.title, e))?;
            skill_ids.push(sid);
        }

        // ── 1b. 注入全域 active skills（injection_mode='active', agent_scope∈{all,sub}）──
        {
            #[derive(serde::Deserialize)]
            struct GlobalSkillId { skill_id: String }
            let mut resp = self.db.query(
                "SELECT skill_id FROM agent_skills \
                 WHERE vault_id = $vid \
                   AND is_active = true \
                   AND injection_mode = 'active' \
                   AND (agent_scope = 'all' OR agent_scope = 'sub')"
            )
            .bind(("vid", vault_id.clone()))
            .await;
            if let Ok(ref mut r) = resp {
                if let Ok(rows) = r.take::<Vec<GlobalSkillId>>(0) {
                    for row in rows {
                        if !skill_ids.contains(&row.skill_id) {
                            skill_ids.push(row.skill_id);
                        }
                    }
                }
            }
        }

        // ── 2. trigger_embedding（0a 已算好，直接複用）────────────────
        let trigger_embedding: Option<Vec<f32>> = if trigger_emb_for_dedup.is_empty() {
            None
        } else {
            Some(trigger_emb_for_dedup)
        };

        // ── 3. INSERT agent_definition ───────────────────────────────
        let def_id = uuid::Uuid::new_v4().to_string();
        let rounds = max_rounds.max(1).min(20);

        self.db.query(
            "INSERT INTO agent_definitions \
             (def_id, vault_id, name, description, kind, skill_ids, tool_names, \
              system_prompt, max_rounds, is_active, is_builtin, trigger, trigger_embedding, created_at) \
             VALUES ($did, $vid, $name, $desc, 'sub', $skills, $tools, \
                     '', $rounds, true, false, $trigger, $temb, time::now())"
        )
        .bind(("did",     def_id.clone()))
        .bind(("vid",     vault_id.clone()))
        .bind(("name",    name.clone()))
        .bind(("desc",    description.clone()))
        .bind(("tools",   tool_names.clone()))
        .bind(("skills",  skill_ids.clone()))
        .bind(("rounds",  rounds))
        .bind(("trigger", trigger.clone()))
        .bind(("temb",    trigger_embedding))
        .await
        .map_err(|e| format!("INSERT agent_definitions 失敗: {e}"))?;

        emit(
            "system_agent:agent_created".into(),
            serde_json::json!({ "def_id": def_id, "name": name }),
        );

        Ok(crate::commands::agent_def::AgentDefinition {
            def_id,
            vault_id,
            name,
            description,
            kind: "sub".to_string(),
            skill_ids,
            tool_names,
            system_prompt: String::new(),
            max_rounds: rounds,
            is_active: true,
            is_builtin: false,
            trigger,
            created_at: chrono::Utc::now().timestamp_millis(),
            status: "active".to_string(),
            slept_at: None,
            use_count: 0,
            last_used_at: None,
        })
    }

    /// 依 title 精確匹配尋找既有 skill；找不到則建立新 skill（含 trigger_embedding）。
    /// 建立的 skill 預設 `is_active = true`，`agent_scope = 'all'`。
    async fn find_or_create_skill(
        &self,
        spec: &NewSkillSpec,
        vault_id: &str,
        emb_url: Option<&str>,
        client: &reqwest::Client,
    ) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct SkillIdRow { skill_id: String }

        // 先找 title 完全匹配（不分大小寫）
        let mut resp = self.db.query(
            "SELECT skill_id FROM agent_skills \
             WHERE vault_id = $vid \
               AND string::lowercase(title) = string::lowercase($title) \
             LIMIT 1"
        )
        .bind(("vid",   vault_id.to_string()))
        .bind(("title", spec.title.clone()))
        .await
        .map_err(|e| e.to_string())?;

        let existing: Vec<SkillIdRow> = resp.take(0).unwrap_or_default();
        if let Some(row) = existing.into_iter().next() {
            return Ok(row.skill_id);
        }

        // 建立新 skill（含 trigger_embedding）
        let skill_id = uuid::Uuid::new_v4().to_string();
        let trigger_embedding: Option<Vec<f32>> = if let Some(url) = emb_url {
            if !spec.trigger.trim().is_empty() {
                let vec = crate::commands::ai::get_embedding(client, url, &spec.trigger).await;
                if vec.is_empty() { None } else { Some(vec) }
            } else { None }
        } else { None };

        let mode = if spec.injection_mode == "active" { "active" } else { "passive" };

        self.db.query(
            "INSERT INTO agent_skills \
             (skill_id, vault_id, knowledge_item_id, title, trigger, behavior, \
              injection_mode, is_active, trigger_embedding, auto_tool_calls, agent_scope, created_at) \
             VALUES ($id, $vid, '', $title, $trigger, $behavior, \
                     $mode, true, $temb, [], 'all', time::now())"
        )
        .bind(("id",      skill_id.clone()))
        .bind(("vid",     vault_id.to_string()))
        .bind(("title",   spec.title.clone()))
        .bind(("trigger", spec.trigger.clone()))
        .bind(("behavior", spec.behavior.clone()))
        .bind(("mode",    mode.to_string()))
        .bind(("temb",    trigger_embedding))
        .await
        .map_err(|e| format!("INSERT agent_skills 失敗: {e}"))?;

        Ok(skill_id)
    }

    /// 從 DB 載入 skill_ids 對應的 behavior 文字，組成注入區塊。
    async fn build_skill_section(&self, skill_ids: &[String], vault_id: &str) -> String {
        if skill_ids.is_empty() {
            return String::new();
        }

        #[derive(serde::Deserialize)]
        struct SkillBehavior {
            title: String,
            trigger: String,
            behavior: String,
        }

        let mut resp = self.db.query(
            "SELECT title, trigger, behavior FROM agent_skills \
             WHERE vault_id = $vid AND skill_id INSIDE $ids AND is_active = true"
        )
        .bind(("vid", vault_id.to_string()))
        .bind(("ids", skill_ids.to_vec()))
        .await.ok();

        let skills: Vec<SkillBehavior> = resp
            .as_mut()
            .and_then(|r| r.take::<Vec<SkillBehavior>>(0).ok())
            .unwrap_or_default();

        if skills.is_empty() {
            return String::new();
        }

        let mut section = String::from("# 使用者技能規範\n");
        for s in &skills {
            section.push_str(&format!(
                "## {}\n觸發條件：{}\n行為規範：{}\n\n",
                s.title, s.trigger, s.behavior
            ));
        }
        section
    }
}

// ── run_sub_agent_with_system（帶自訂 system prompt + max_rounds）────────────

async fn run_sub_agent_with_system(
    sub_session_id: String,
    parent_session_id: String,
    def_name: &str,
    task: &str,
    system_content: &str,
    filtered_tools: Value,
    registry: Arc<ToolRegistry>,
    llm_fn: LlmFn,
    emit: EmitEventFn,
    max_rounds: usize,
    cancel: Option<Arc<AtomicBool>>,
) -> String {
    use crate::runtime::dispatcher::Dispatcher;
    use crate::runtime::planner::Planner;
    use crate::runtime::transaction::Transaction;

    let mut messages: Vec<Value> = vec![
        serde_json::json!({"role": "system", "content": system_content}),
        serde_json::json!({"role": "user", "content": task}),
    ];

    emit(
        "sub_agent:start".into(),
        serde_json::json!({
            "sub_session_id": sub_session_id,
            "parent_session_id": parent_session_id,
            "kind": def_name,
            "task": task,
        }),
    );

    let dispatcher = Dispatcher::new(Arc::clone(&registry));
    let tx = Arc::new(Transaction::new());
    let _ = tx.prepare().await;

    let mut final_text = String::new();

    for round in 0..max_rounds {
        // 取消檢查
        if let Some(ref flag) = cancel {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                emit(
                    "sub_agent:cancelled".into(),
                    serde_json::json!({ "sub_session_id": sub_session_id }),
                );
                return "[sub-agent 已取消]".to_string();
            }
        }

        let tools_opt = if filtered_tools.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            Some(filtered_tools.clone())
        } else {
            None
        };

        let llm_result = match llm_fn(messages.clone(), tools_opt, cancel.clone()).await {
            Ok(r) => r,
            Err(e) => {
                emit(
                    "sub_agent:error".into(),
                    serde_json::json!({ "sub_session_id": sub_session_id, "error": e }),
                );
                return format!("[sub-agent 錯誤] {}", e);
            }
        };

        final_text = llm_result.full_text.clone();

        if llm_result.tool_calls.is_empty() {
            break;
        }

        for (_, name, args) in &llm_result.tool_calls {
            emit(
                "sub_agent:tool_call".into(),
                serde_json::json!({
                    "sub_session_id": sub_session_id,
                    "parent_session_id": parent_session_id,
                    "kind": def_name,
                    "display": format!("[{def_name}] {name}"),
                    "args": args,
                }),
            );
        }

        let tool_graph = Planner::plan(&llm_result.tool_calls);
        let results = match dispatcher.run(Arc::clone(&tx), tool_graph).await {
            Ok(r) => r,
            Err(e) => {
                emit(
                    "sub_agent:error".into(),
                    serde_json::json!({ "sub_session_id": sub_session_id, "error": e }),
                );
                return format!("[sub-agent tool error] {}", e);
            }
        };

        let tc_json: Vec<Value> = llm_result.tool_calls.iter().map(|(id, name, args)| {
            serde_json::json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()}
            })
        }).collect();

        messages.push(serde_json::json!({
            "role": "assistant",
            "content": null,
            "tool_calls": tc_json,
        }));

        for ((tool_id, name, _), result) in llm_result.tool_calls.iter().zip(results.iter()) {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_id,
                "name": name,
                "content": result.as_str().unwrap_or(&result.to_string()),
            }));
        }

        if round == max_rounds - 1 {
            final_text = format!("[達最大輪數 {max_rounds}，部分結果] {}", final_text);
        }
    }

    let _ = tx.commit().await;

    emit(
        "sub_agent:done".into(),
        serde_json::json!({
            "sub_session_id": sub_session_id,
            "parent_session_id": parent_session_id,
            "kind": def_name,
            "result_preview": final_text.chars().take(200).collect::<String>(),
        }),
    );

    final_text
}
