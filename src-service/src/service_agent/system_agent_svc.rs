// service_agent/system_agent_svc.rs
//
// SystemAgentService — rule-based broker，不使用 LLM。
//
// 職責：
// 1. 維護 agent_definitions 查詢（從 DB 載入）
// 2. 路由 call_agent 請求到對應 definition
// 3. 執行 sub-agent 並同步回傳結果
// 4. 記錄所有請求到 debug event stream
// 5. 追蹤本次 session 的 ephemeral agents

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::db::SurrealDb;
use super::types::{ConfirmWriteFn, EmitEventFn, EmbedFn, LlmFn, NewSkillSpec, SummarizeFn};

// ── AgentDefinition (local DB row) ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub def_id: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub skill_ids: Vec<String>,
    pub tool_names: Vec<String>,
    pub system_prompt: String,
    pub max_rounds: i64,
    pub trigger: String,
    pub trigger_embedding: Vec<f32>,
    pub is_builtin: bool,
}

fn agent_from_row(row: &Value) -> AgentDefinition {
    let skill_ids: Vec<String> = row["skill_ids"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let tool_names: Vec<String> = row["tool_names"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let trigger_embedding: Vec<f32> = row["trigger_embedding"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
        .unwrap_or_default();
    AgentDefinition {
        def_id: row["def_id"].as_str().unwrap_or("").to_string(),
        name: row["name"].as_str().unwrap_or("").to_string(),
        description: row["description"].as_str().unwrap_or("").to_string(),
        kind: row["kind"].as_str().unwrap_or("sub").to_string(),
        skill_ids,
        tool_names,
        system_prompt: row["system_prompt"].as_str().unwrap_or("").to_string(),
        max_rounds: row["max_rounds"].as_i64().unwrap_or(5),
        trigger: row["trigger"].as_str().unwrap_or("").to_string(),
        trigger_embedding,
        is_builtin: row["is_builtin"].as_bool().unwrap_or(false),
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { return 0.0; }
    dot / (na * nb)
}

// ── Ephemeral agent record (本次 session 中動態建立的 agent) ──────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EphemeralAgent {
    pub sub_session_id: String,
    pub def_id: String,
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
    pub context: String,  // 序列化的對話歷史 + 記憶摘要（注入 sub-agent system prompt）
    /// 呼叫端的 ORCHESTRATOR_SYSTEM：sub-agent 若無自訂 system_prompt，繼承此 prompt
    pub parent_system: String,
    pub conversation_id: Option<String>,  // 用於 note-open pending_plan 儲存
    pub vault_path: String,               // 用於 note_refs 路徑解析
}

/// Sub-agent 工具執行所需的 vault 上下文
pub struct SubAgentVaultCtx {
    pub client: reqwest::Client,
    pub llm_url: String,
    pub db: SurrealDb,
    pub vault_id: String,
    pub account_id: String,
    pub vault_path: String,
    pub embedding_url: Option<String>,
}

// ── SystemAgentService ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SystemAgentService {
    db: SurrealDb,
    account_id: Arc<RwLock<String>>,
    vault_id: Arc<RwLock<String>>,
    /// 本次 session 的 ephemeral agents（供 UI 顯示）
    pub ephemeral: Arc<RwLock<Vec<EphemeralAgent>>>,
}

impl SystemAgentService {
    pub fn new(db: SurrealDb) -> Self {
        Self {
            db,
            account_id: Arc::new(RwLock::new(String::new())),
            vault_id: Arc::new(RwLock::new(String::new())),
            ephemeral: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn set_vault_id(&self, vid: String) {
        *self.vault_id.write().await = vid;
    }

    pub async fn set_account_id(&self, aid: String) {
        *self.account_id.write().await = aid;
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
        llm_fn: LlmFn,
        emit: EmitEventFn,
        cancel: Option<Arc<AtomicBool>>,
        embed_fn: Option<EmbedFn>,
        confirm_write: ConfirmWriteFn,
        summarize_fn: Option<SummarizeFn>,
        vault_ctx: SubAgentVaultCtx,
    ) -> String {
        let vault_id = self.vault_id.read().await.clone();
        let account_id = self.account_id.read().await.clone();

        // ── 1. 找 definition（name 模糊匹配 → embedding fallback，含 sleep agent）──
        let def = {
            let by_name = self.find_agent_definition_by_name(&vault_id, &account_id, &request.target).await;
            if by_name.is_some() {
                by_name
            } else if let Some(ref ef) = embed_fn {
                let target_emb = ef(request.target.clone()).await;
                if !target_emb.is_empty() {
                    self.find_matching_agent_definition(&vault_id, &account_id, &target_emb, 0.55).await
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
        if let Some(ref ef) = embed_fn {
            let task_emb = ef(request.task.clone()).await;
            if !task_emb.is_empty() {
                let skill_rows = self.load_all_skills(&vault_id, &account_id).await;
                for row in &skill_rows {
                    if !row["is_active"].as_bool().unwrap_or(true) { continue; }
                    let scope = row["agent_scope"].as_str().unwrap_or("all");
                    if scope != "all" && scope != "sub" { continue; }
                    let sid = row["skill_id"].as_str().unwrap_or("");
                    if sid.is_empty() || skill_ids.iter().any(|s| s == sid) { continue; }
                    let emb: Vec<f32> = row["trigger_embedding"].as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
                        .unwrap_or_default();
                    if emb.is_empty() { continue; }
                    let sim = cosine_similarity(&task_emb, &emb);
                    if sim >= 0.60 {
                        skill_ids.push(sid.to_string());
                    }
                }
            }
        }

        // ── 3b. 組建 system prompt（加入 skills 注入 + override）─────────────────
        let skill_section = if !skill_ids.is_empty() {
            self.build_skill_section(&skill_ids, &vault_id, &account_id).await
        } else {
            String::new()
        };

        let anti_hallucination = "\n\n## 規則（必須遵守）\n\
            1. 必須實際呼叫工具執行任務；禁止假裝已完成或虛構結果。\n\
            2. 若搜尋無結果，直接回報找不到，不可捏造內容或筆記名稱。\n\
            3. 完成後簡潔回傳結果，不加多餘說明。";

        let base_system = if !system_prompt_override.is_empty() {
            // Agent definition 有自訂 system prompt → 優先使用
            format!("{}{}", system_prompt_override, anti_hallucination)
        } else if !request.parent_system.is_empty() {
            // 繼承主 LLM 的 ORCHESTRATOR_SYSTEM，讓 sub-agent 有相同的意圖理解框架
            format!("{}{}", request.parent_system, anti_hallucination)
        } else {
            // Fallback：無任何 system → 使用內建 vault 意圖指南
            format!(
                "你是 NoteTreeLM 的 {def_name} sub-agent，在使用者的 Markdown 筆記庫（Vault）中工作。\n\
                 \n\
                 ## 常見使用者意圖解讀\n\
                 - 「打開 X」「看 X」→ 先用 search_vault 搜尋關鍵字，再用 open_note 打開\n\
                 - 「讀取 X」「X 裡面有什麼」→ 先用 search_vault，再用 read_note 取得內容\n\
                 - 「找 X」「搜尋 X」→ 用 search_vault\n\
                 - 「建立 X」「新增 X」→ 用 create_note\n\
                 - 「更新 X」「修改 X」→ 先搜尋確認路徑，再用 update_note\n\
                 - 使用者提到的名詞通常是筆記名稱關鍵字，不一定完全吻合，需用 search_vault 模糊搜尋{}",
                anti_hallucination
            )
        };

        let full_system = if skill_section.is_empty() {
            base_system
        } else {
            format!("{base_system}\n\n{skill_section}")
        };

        // ── 4. 記錄 ephemeral agent ───────────────────────────────────────────────
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
            &request.context,
            &full_system,
            filtered_tools,
            llm_fn,
            emit.clone(),
            max_rounds,
            cancel,
            confirm_write,
            request.vault_path,
            request.conversation_id,
            embed_fn,
            summarize_fn,
            vault_ctx,
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
        if !def_id.starts_with("ephemeral-") {
            self.record_agent_usage(&account_id, &def_id).await;
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
        system_prompt: String,
        skills: Vec<NewSkillSpec>,
        max_rounds: i64,
        embed_fn: Option<&EmbedFn>,
        emit: EmitEventFn,
        user_ask: &str,
    ) -> Result<AgentDefinition, String> {
        let vault_id = self.vault_id.read().await.clone();
        if vault_id.is_empty() {
            return Err("Vault 未設定".to_string());
        }
        let account_id = self.account_id.read().await.clone();

        // 1. 用 user_ask embedding 比對現有 agent（閾值 0.75，包含 sleep agent）
        if let Some(ef) = embed_fn {
            let ask_emb = ef(user_ask.to_string()).await;
            if !ask_emb.is_empty() {
                if let Some(existing) = self.find_matching_agent_definition(&vault_id, &account_id, &ask_emb, 0.75).await {
                    return Ok(existing);
                }
            }
        }

        // 2. 找不到相似 agent → 建立新的
        self.create_agent(name, description, trigger, tool_names, system_prompt, skills, max_rounds, embed_fn, emit).await
    }

    /// 由 `create_agent` 工具呼叫，Chat LLM 透過此方法有機地建立專用 agent。
    pub async fn create_agent(
        &self,
        name: String,
        description: String,
        trigger: String,
        tool_names: Vec<String>,
        system_prompt: String,
        skills: Vec<NewSkillSpec>,
        max_rounds: i64,
        embed_fn: Option<&EmbedFn>,
        emit: EmitEventFn,
    ) -> Result<AgentDefinition, String> {
        let vault_id = self.vault_id.read().await.clone();
        if vault_id.is_empty() {
            return Err("Vault 未設定，無法建立 Agent".to_string());
        }
        let account_id = self.account_id.read().await.clone();

        // ── 0a. 計算 trigger embedding（後續 dedup 用）─────────────────
        let trigger_emb_for_dedup: Vec<f32> = if let Some(ef) = embed_fn {
            if !trigger.trim().is_empty() {
                ef(trigger.clone()).await
            } else { vec![] }
        } else { vec![] };

        // ── 0b. Embedding dedup：語意相似度 > 0.85 → 複用現有 agent ──
        if !trigger_emb_for_dedup.is_empty() {
            if let Some(existing) = self.find_matching_agent_definition(&vault_id, &account_id, &trigger_emb_for_dedup, 0.85).await {
                emit(
                    "system_agent:agent_created".into(),
                    serde_json::json!({ "def_id": existing.def_id, "name": existing.name, "reused": true }),
                );
                return Ok(existing);
            }
        }

        // ── 0c. 同名 dedup：直接複用，不報錯 ────────────────────────
        if let Some(existing) = self.find_agent_definition_by_name(&vault_id, &account_id, &name).await {
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
                .find_or_create_skill(spec, &vault_id, &account_id, embed_fn)
                .await
                .map_err(|e| format!("skill '{}' 建立失敗: {}", spec.title, e))?;
            skill_ids.push(sid);
        }

        // ── 1b. 注入全域 active skills（injection_mode='active', agent_scope∈{all,sub}）──
        {
            let global_rows = self.load_all_skills(&vault_id, &account_id).await;
            for row in &global_rows {
                if !row["is_active"].as_bool().unwrap_or(true) { continue; }
                let mode = row["injection_mode"].as_str().unwrap_or("");
                if mode != "active" { continue; }
                let scope = row["agent_scope"].as_str().unwrap_or("all");
                if scope != "all" && scope != "sub" { continue; }
                let sid = row["skill_id"].as_str().unwrap_or("").to_string();
                if !sid.is_empty() && !skill_ids.contains(&sid) {
                    skill_ids.push(sid);
                }
            }
        }

        // ── 2. trigger_embedding（0a 已算好，直接複用）────────────────
        let trigger_embedding: Option<Vec<f32>> = if trigger_emb_for_dedup.is_empty() {
            None
        } else {
            Some(trigger_emb_for_dedup)
        };

        // ── 3. INSERT agent_definition to DB ─────────────────────────
        let rounds = max_rounds.max(1).min(20);
        let def_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();

        self.db
            .query("INSERT INTO agent_definitions (def_id, account_id, name, description, kind, skill_ids, tool_names, system_prompt, max_rounds, is_active, is_builtin, trigger, created_at) VALUES ($did, $aid, $name, $desc, $kind, $skills, $tools, $prompt, $rounds, true, false, $trigger, $now)")
            .bind(("did", def_id.clone()))
            .bind(("aid", account_id.clone()))
            .bind(("name", name.clone()))
            .bind(("desc", description.clone()))
            .bind(("kind", "sub".to_string()))
            .bind(("skills", serde_json::to_value(&skill_ids).unwrap_or(serde_json::json!([]))))
            .bind(("tools", serde_json::to_value(&tool_names).unwrap_or(serde_json::json!([]))))
            .bind(("prompt", system_prompt.clone()))
            .bind(("rounds", rounds))
            .bind(("trigger", trigger.clone()))
            .bind(("now", now))
            .await
            .map_err(|e| format!("INSERT agent_definitions 失敗: {e}"))?;

        // Update trigger_embedding separately if available
        if let Some(ref emb) = trigger_embedding {
            let _ = self.db
                .query("UPDATE agent_definitions SET trigger_embedding = $emb WHERE def_id = $did")
                .bind(("emb", serde_json::to_value(emb).unwrap_or(serde_json::Value::Null)))
                .bind(("did", def_id.clone()))
                .await;
        }

        emit(
            "system_agent:agent_created".into(),
            serde_json::json!({ "def_id": def_id, "name": name }),
        );

        Ok(AgentDefinition {
            def_id,
            name,
            description,
            kind: "sub".to_string(),
            skill_ids,
            tool_names,
            system_prompt,
            max_rounds: rounds,
            trigger,
            trigger_embedding: trigger_embedding.unwrap_or_default(),
            is_builtin: false,
        })
    }

    /// 依 title 精確匹配尋找既有 skill；找不到則建立新 skill（含 trigger_embedding）。
    /// 建立的 skill 預設 `is_active = true`，`agent_scope = 'all'`。
    async fn find_or_create_skill(
        &self,
        spec: &NewSkillSpec,
        vault_id: &str,
        account_id: &str,
        embed_fn: Option<&EmbedFn>,
    ) -> Result<String, String> {
        // 先找 title 完全匹配（不分大小寫）
        let skill_rows = self.load_all_skills(vault_id, account_id).await;
        let title_lower = spec.title.to_lowercase();
        if let Some(row) = skill_rows.iter().find(|r| {
            r["title"].as_str().unwrap_or("").to_lowercase() == title_lower
        }) {
            if let Some(sid) = row["skill_id"].as_str() {
                return Ok(sid.to_string());
            }
        }

        // 建立新 skill（含 trigger_embedding）
        let skill_id = uuid::Uuid::new_v4().to_string();
        let trigger_embedding: Option<Vec<f32>> = if let Some(ef) = embed_fn {
            if !spec.trigger.trim().is_empty() {
                let vec = ef(spec.trigger.clone()).await;
                if vec.is_empty() { None } else { Some(vec) }
            } else { None }
        } else { None };

        let mode = if spec.injection_mode == "active" { "active" } else { "passive" };
        let now = chrono::Utc::now().timestamp();

        self.db
            .query("INSERT INTO agent_skills (skill_id, account_id, knowledge_item_id, title, trigger, behavior, tool_calls, is_active, trigger_count, injection_mode, agent_scope, need_tool_chain, tool_chain_order, created_at) VALUES ($sid, $aid, $kiid, $title, $trigger, $behavior, $tc, true, 0, $imode, $scope, $need_chain, $chain_order, $now)")
            .bind(("sid", skill_id.clone()))
            .bind(("aid", account_id.to_string()))
            .bind(("kiid", String::new()))
            .bind(("title", spec.title.clone()))
            .bind(("trigger", spec.trigger.clone()))
            .bind(("behavior", spec.behavior.clone()))
            .bind(("tc", serde_json::json!([])))
            .bind(("imode", mode.to_string()))
            .bind(("scope", "all".to_string()))
            .bind(("need_chain", spec.need_tool_chain))
            .bind(("chain_order", serde_json::to_value(&spec.tool_chain_order).unwrap_or(serde_json::json!([]))))
            .bind(("now", now))
            .await
            .map_err(|e| format!("INSERT agent_skills 失敗: {e}"))?;

        if let Some(emb) = trigger_embedding {
            let _ = self.db
                .query("UPDATE agent_skills SET trigger_embedding = $emb WHERE skill_id = $sid")
                .bind(("emb", serde_json::to_value(&emb).unwrap_or(serde_json::Value::Null)))
                .bind(("sid", skill_id.clone()))
                .await;
        }

        Ok(skill_id)
    }

    /// 從 DB 載入 skill_ids 對應的 behavior 文字，組成注入區塊。
    async fn build_skill_section(&self, skill_ids: &[String], vault_id: &str, account_id: &str) -> String {
        if skill_ids.is_empty() {
            return String::new();
        }

        let skill_rows = self.load_all_skills(vault_id, account_id).await;

        let matching: Vec<&Value> = skill_rows.iter().filter(|r| {
            if !r["is_active"].as_bool().unwrap_or(true) { return false; }
            let sid = r["skill_id"].as_str().unwrap_or("");
            skill_ids.iter().any(|id| id == sid)
        }).collect();

        if matching.is_empty() {
            return String::new();
        }

        let mut section = String::from("# 使用者技能規範\n");
        for r in &matching {
            let title = r["title"].as_str().unwrap_or("");
            let trigger = r["trigger"].as_str().unwrap_or("");
            let behavior = r["behavior"].as_str().unwrap_or("");
            let need_chain = r["need_tool_chain"].as_bool().unwrap_or(false);
            let chain_order: Vec<&str> = if need_chain {
                r["tool_chain_order"].as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default()
            } else {
                vec![]
            };
            if need_chain && !chain_order.is_empty() {
                section.push_str(&format!(
                    "## {}\n觸發條件：{}\n行為規範：{}\n工具執行順序：{}\n\n",
                    title, trigger, behavior, chain_order.join(" → ")
                ));
            } else {
                section.push_str(&format!(
                    "## {}\n觸發條件：{}\n行為規範：{}\n\n",
                    title, trigger, behavior
                ));
            }
        }
        section
    }

    // ── DB helpers ────────────────────────────────────────────────────────────

    async fn find_agent_definition_by_name(&self, vault_id: &str, account_id: &str, name: &str) -> Option<AgentDefinition> {
        let mut resp = self.db
            .query("SELECT *, record::id(id) AS id FROM agent_definitions WHERE (account_id = $aid OR vault_id = $vid) AND name = $name LIMIT 1")
            .bind(("aid", account_id.to_string()))
            .bind(("vid", vault_id.to_string()))
            .bind(("name", name.to_string()))
            .await
            .ok()?;
        let rows: Vec<Value> = resp.take(0).ok()?;
        rows.into_iter().next().map(|r| agent_from_row(&r))
    }

    async fn find_matching_agent_definition(
        &self,
        vault_id: &str,
        account_id: &str,
        target_emb: &[f32],
        threshold: f32,
    ) -> Option<AgentDefinition> {
        let mut resp = self.db
            .query("SELECT *, record::id(id) AS id FROM agent_definitions WHERE account_id = $aid OR vault_id = $vid")
            .bind(("aid", account_id.to_string()))
            .bind(("vid", vault_id.to_string()))
            .await
            .ok()?;
        let rows: Vec<Value> = resp.take(0).ok()?;
        let mut best: Option<(f32, AgentDefinition)> = None;
        for row in &rows {
            let emb: Vec<f32> = row["trigger_embedding"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
                .unwrap_or_default();
            if emb.is_empty() { continue; }
            let sim = cosine_similarity(target_emb, &emb);
            if sim >= threshold {
                if best.as_ref().map_or(true, |(s, _)| sim > *s) {
                    best = Some((sim, agent_from_row(row)));
                }
            }
        }
        best.map(|(_, d)| d)
    }

    async fn load_all_skills(&self, _vault_id: &str, account_id: &str) -> Vec<Value> {
        let mut resp = match self.db
            .query("SELECT *, record::id(id) AS id FROM agent_skills WHERE account_id = $aid AND is_active = true")
            .bind(("aid", account_id.to_string()))
            .await
        {
            Ok(r) => r,
            Err(_) => return vec![],
        };
        resp.take(0).unwrap_or_default()
    }

    async fn record_agent_usage(&self, account_id: &str, def_id: &str) {
        let now = chrono::Utc::now().timestamp();
        let _ = self.db
            .query("UPDATE agent_definitions SET use_count = (use_count OR 0) + 1, last_used_at = $now, status = 'active', slept_at = NONE WHERE def_id = $did AND account_id = $aid")
            .bind(("now", now))
            .bind(("did", def_id.to_string()))
            .bind(("aid", account_id.to_string()))
            .await;
    }
}

// ── run_sub_agent_with_system（帶自訂 system prompt + max_rounds）────────────

/// search_vault / read_note 執行結果中提取筆記路徑（與 interactive.rs 邏輯一致）
fn extract_note_refs(tool_name: &str, args: &Value, result: &str, vault_path: &str) -> Vec<String> {
    if vault_path.is_empty() { return vec![]; }
    match tool_name {
        "read_note" => {
            if let Some(p) = args["path"].as_str() {
                let abs = std::path::PathBuf::from(vault_path).join(p);
                vec![abs.to_string_lossy().to_string()]
            } else { vec![] }
        }
        "search_vault" => result.lines().filter_map(|line| {
            let line = line.trim();
            if line.starts_with("- **") {
                if let Some(lp) = line.rfind('(') {
                    if let Some(rp) = line[lp..].find(')') {
                        let rel = &line[lp + 1..lp + rp];
                        if rel.ends_with(".md") {
                            let abs = std::path::PathBuf::from(vault_path).join(rel);
                            let abs_path = abs.to_string_lossy().to_string();
                            let section = line.find("** (").and_then(|bold_end| {
                                let content = &line[4..bold_end];
                                content.find(" § ").map(|sep| content[sep + 3..].to_string())
                            });
                            return Some(match section {
                                Some(s) if !s.is_empty() => format!("{}#{}", abs_path, s),
                                _ => abs_path,
                            });
                        }
                    }
                }
            }
            None
        }).collect(),
        _ => vec![],
    }
}

fn is_write_tool(name: &str) -> bool {
    matches!(name, "create_note" | "update_note" | "create_folder")
}

fn tool_display_sub(name: &str, args: &Value) -> String {
    match name {
        "create_note"   => format!("✏️  建立筆記: {}", args["path"].as_str().unwrap_or("")),
        "update_note"   => format!("✏️  更新筆記: {}", args["path"].as_str().unwrap_or("")),
        "create_folder" => format!("📁 建立資料夾: {}", args["path"].as_str().unwrap_or("")),
        _ => format!("[{}]", name),
    }
}

/// Save deferred tools to pending_plans table directly (service-side)
async fn save_pending_plan_db(db: &SurrealDb, conv_id: &str, deferred: &[Value]) {
    let tools_json = serde_json::to_string(deferred).unwrap_or_else(|_| "[]".to_string());
    let now = chrono::Utc::now().timestamp();
    let _ = db
        .query("INSERT INTO pending_plans (conversation_id, deferred_tools_json, created_at) VALUES ($cid, $tools, $now) ON DUPLICATE KEY UPDATE deferred_tools_json = $tools, created_at = $now")
        .bind(("cid", conv_id.to_string()))
        .bind(("tools", tools_json))
        .bind(("now", now))
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_sub_agent_with_system(
    sub_session_id: String,
    parent_session_id: String,
    def_name: &str,
    task: &str,
    context: &str,
    system_content: &str,
    filtered_tools: Value,
    llm_fn: LlmFn,
    emit: EmitEventFn,
    max_rounds: usize,
    cancel: Option<Arc<AtomicBool>>,
    confirm_write: ConfirmWriteFn,
    vault_path: String,
    conversation_id: Option<String>,
    embed_fn: Option<EmbedFn>,
    summarize_fn: Option<SummarizeFn>,
    vault_ctx: SubAgentVaultCtx,
) -> String {
    use super::dispatcher::Dispatcher;
    use super::planner::Planner;
    use super::tool_registry::ToolRegistry;
    use super::transaction::Transaction;
    use super::types::Tool;

    let mut messages: Vec<Value> = vec![
        serde_json::json!({"role": "system", "content": system_content}),
    ];
    // context 注入：序列化的對話歷史 + 記憶摘要
    if !context.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": format!("[對話背景]\n{}", context)
        }));
    }
    messages.push(serde_json::json!({"role": "user", "content": task}));

    emit(
        "sub_agent:start".into(),
        serde_json::json!({
            "sub_session_id": sub_session_id,
            "parent_session_id": parent_session_id,
            "kind": def_name,
            "task": task,
        }),
    );

    // Build ToolRegistry with closures that call dispatch_interactive_tool
    let mut registry = ToolRegistry::new();
    let tool_names = [
        "list_structure", "read_note", "create_note", "update_note",
        "create_folder", "search_vault", "query_memory", "open_note",
        "delete_note", "delete_folder", "move_note", "append_to_note",
    ];

    for &name in &tool_names {
        let client = vault_ctx.client.clone();
        let llm_url = vault_ctx.llm_url.clone();
        let db = vault_ctx.db.clone();
        let vault_id = vault_ctx.vault_id.clone();
        let account_id = vault_ctx.account_id.clone();
        let vault_path_c = vault_ctx.vault_path.clone();
        let embedding_url = vault_ctx.embedding_url.clone();
        let name_owned = name.to_string();

        let execute_fn: super::types::ToolFn = Arc::new(move |args: Value| {
            let client = client.clone();
            let llm_url = llm_url.clone();
            let db = db.clone();
            let vault_id = vault_id.clone();
            let account_id = account_id.clone();
            let vault_path_c = vault_path_c.clone();
            let embedding_url = embedding_url.clone();
            let name_owned = name_owned.clone();
            Box::pin(async move {
                match super::vault_tools::dispatch_interactive_tool(
                    &client, &llm_url, &db,
                    &vault_id, &account_id, &vault_path_c, &embedding_url,
                    &name_owned, &args,
                ).await {
                    Ok((v, _rollback)) => Ok(v),
                    Err(e) => Err(e),
                }
            })
        });

        registry.register(name.to_string(), Tool {
            execute: execute_fn,
            rollback: None,
        });
    }

    let registry = Arc::new(registry);
    // system_agent_svc handles its own emit + write confirmation outside Dispatcher;
    // pass no-ops so Executor doesn't duplicate pre-tool SSE or confirmation logic.
    let noop_emit: super::types::EmitEventFn = Arc::new(|_, _| {});
    let no_write: super::types::IsWriteFn = Arc::new(|_| false);
    let dispatcher = Dispatcher::new(Arc::clone(&registry), noop_emit, no_write);
    let tx = Arc::new(Transaction::new());

    let mut final_text = String::new();
    let mut all_note_refs: Vec<String> = Vec::new();

    for round in 0..max_rounds {
        // 取消檢查
        if let Some(ref flag) = cancel {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = tx.cancel().await;
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

        // ── 寫入工具確認 ──────────────────────────────────────────────────────
        // 有 conversation_id + embed_fn → 儲存 pending_plan，讓使用者在 chat 用自然語言確認
        // 無 conversation_id（直接工具呼叫）→ 用 confirm_write fn（UI dialog）
        let has_write = llm_result.tool_calls.iter().any(|(_, n, _)| is_write_tool(n));
        if has_write {
            let batch_display = llm_result.tool_calls.iter()
                .filter(|(_, n, _)| is_write_tool(n))
                .map(|(_, n, a)| format!("- {}", tool_display_sub(n, a)))
                .collect::<Vec<_>>()
                .join("\n");

            if let (Some(ref conv_id), Some(ref _ef)) = (&conversation_id, &embed_fn) {
                // pending_plan 路徑：儲存 deferred 工具，sub-agent 先回傳確認訊息
                let deferred: Vec<Value> = llm_result.tool_calls.iter()
                    .filter(|(_, n, _)| is_write_tool(n))
                    .map(|(_, name, args)| serde_json::json!({"name": name, "args": args}))
                    .collect();
                save_pending_plan_db(&vault_ctx.db, conv_id, &deferred).await;

                // 讓 LLM 知道寫入被暫緩，並傳達確認訊息給使用者
                let confirm_prompt = format!(
                    "以下操作已暫緩，等待你的確認（請回覆「確認」或「取消」）：\n{}",
                    batch_display
                );
                let _ = tx.cancel().await; // 本輪 transaction 暫緩
                final_text = confirm_prompt.clone();
                emit("llm:token".into(), Value::String(confirm_prompt.clone()));
                break; // 停止本輪 sub-agent，等待使用者確認
            } else {
                // UI dialog 路徑（無對話脈絡）
                emit("agent:write_request".into(), Value::String(batch_display.clone()));
                let approved = confirm_write(batch_display).await;
                if !approved {
                    let tc_json: Vec<Value> = llm_result.tool_calls.iter().map(|(id, name, args)| {
                        serde_json::json!({"id": id, "type": "function",
                            "function": {"name": name, "arguments": args.to_string()}})
                    }).collect();
                    messages.push(serde_json::json!({"role": "assistant", "content": null, "tool_calls": tc_json}));
                    for (tool_id, name, _) in &llm_result.tool_calls {
                        messages.push(serde_json::json!({
                            "role": "tool", "tool_call_id": tool_id,
                            "name": name, "content": "用戶拒絕了此寫入操作。"
                        }));
                    }
                    continue;
                }
            }
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

        for ((tool_id, name, args), result) in llm_result.tool_calls.iter().zip(results.iter()) {
            let result_owned = result.as_str().map(String::from).unwrap_or_else(|| result.to_string());
            // read_note 大型結果：嘗試並行分段摘要；其他工具截斷至 3000 chars
            const MAX_TOOL_RESULT: usize = 3000;
            let content = if result_owned.chars().count() > MAX_TOOL_RESULT {
                if name == "read_note" {
                    if let (Some(ref sfn), Some(fp)) = (&summarize_fn, args["path"].as_str()) {
                        match sfn(fp.to_string(), task.to_string()).await {
                            Some(s) => s,
                            None => {
                                let t: String = result_owned.chars().take(MAX_TOOL_RESULT).collect();
                                format!("{}…（已截斷）", t)
                            }
                        }
                    } else {
                        let t: String = result_owned.chars().take(MAX_TOOL_RESULT).collect();
                        format!("{}…（已截斷）", t)
                    }
                } else {
                    let t: String = result_owned.chars().take(MAX_TOOL_RESULT).collect();
                    format!("{}…（已截斷，如需完整請分段呼叫）", t)
                }
            } else {
                result_owned.clone()
            };
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_id,
                "name": name,
                "content": content,
            }));
            // 收集 note_refs（search_vault / read_note）
            let refs = extract_note_refs(name, args, &content, &vault_path);
            if !refs.is_empty() {
                emit("agent:note_refs".into(),
                    serde_json::to_value(refs.clone()).unwrap_or(Value::Null));
                all_note_refs.extend(refs);
            }
        }

        // open_note 是 terminal tool：執行完直接結束，不需再呼叫 LLM
        if llm_result.tool_calls.iter().any(|(_, n, _)| n == "open_note") {
            let opened: Vec<&str> = llm_result.tool_calls.iter()
                .filter(|(_, n, _)| n == "open_note")
                .map(|(_, _, a)| a["path"].as_str().unwrap_or("筆記"))
                .collect();
            final_text = format!("已為你打開：{}", opened.join("、"));
            break;
        }

        if round == max_rounds - 1 {
            final_text = format!("[達最大輪數 {max_rounds}，部分結果] {}", final_text);
        }
    }

    let _ = tx.commit().await;

    // ── note-open pending_plan：找到筆記 → 儲存計畫，讓主對話下一輪確認 ────
    if !all_note_refs.is_empty() {
        if let Some(conv_id) = &conversation_id {
            all_note_refs.dedup();
            let deferred = vec![serde_json::json!({
                "name": "__open_note__",
                "args": { "paths": all_note_refs },
            })];
            save_pending_plan_db(&vault_ctx.db, conv_id, &deferred).await;
        }
    }

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
