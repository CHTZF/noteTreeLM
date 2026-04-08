use crate::{error::AppError, state::AppState, vault};
use crate::api_client::{daemon_get, daemon_post};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{AppHandle, State};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub system_current_vault_path: String,
    pub personal_current_vault_path: String,
    pub theme: String,
    pub auto_save_mode: String,
    pub auto_save_delay: u32,
    pub whisper_cli_path: String,
    pub whisper_model_path: String,
    pub whisper_threads: u32,
    pub whisper_auto_insert: bool,
    pub import_max_depth: u32,
    pub import_max_pages: u32,
    pub ai_enable_topics: bool,
    pub ai_enable_summary: bool,
    pub ai_enable_vision: bool,
    pub llm_model_path: String,
    pub llama_cli_path: String,
    pub embedding_model_path: String,
    pub last_open_note: String,
    pub onboarding_done: bool,
    pub recent_vaults: Vec<String>,
    pub sidebar_width: u32,
    pub graph_panel_width: u32,
    pub sort_orders: String,
    pub font_sans: String,
    pub font_mono: String,
    pub editor_font_size: u32,
    pub ui_font_size: u32,
    pub graph_font_size: u32,
    pub debug_mode: bool,
    pub voice_process_mode: String,
    pub voice_preview_enabled: bool,
    pub voice_noise_suppression: bool,
    pub voice_preview_interval: u32,
    pub enable_chat: bool,
    pub enable_auto_memory: bool,
    pub memory_threshold: u32,
    pub write_confirm_mode: String,
    pub chat_auto_include_note: bool,
}

fn s(map: &HashMap<String, String>, key: &str, default: &str) -> String {
    map.get(key).cloned().unwrap_or_else(|| default.to_string())
}

fn b(map: &HashMap<String, String>, key: &str, default: bool) -> bool {
    map.get(key).map(|v| v == "true").unwrap_or(default)
}

fn u(map: &HashMap<String, String>, key: &str, default: u32) -> u32 {
    map.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>, username: String) -> Result<Settings, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let sys: HashMap<String, String> = daemon_get(&state.http_client, "/settings", tok)
        .await.unwrap_or_default();
    let usr: HashMap<String, String> = daemon_get(
        &state.http_client,
        &format!("/settings/user?username={}", urlencoding::encode(&username)),
        tok,
    ).await.unwrap_or_default();

    let recent_vaults: Vec<String> = serde_json::from_str(&s(&usr, "recent_vaults", "[]")).unwrap_or_default();

    Ok(Settings {
        system_current_vault_path: s(&sys, "system_current_vault_path", ""),
        personal_current_vault_path: s(&usr, "personal_current_vault_path", ""),
        theme: s(&usr, "theme", "dark"),
        auto_save_mode: s(&usr, "auto_save_mode", "afterDelay"),
        auto_save_delay: u(&usr, "auto_save_delay", 1000),
        whisper_cli_path: s(&sys, "whisper_cli_path", ""),
        whisper_model_path: s(&sys, "whisper_model_path", ""),
        whisper_threads: u(&sys, "whisper_threads", 4),
        whisper_auto_insert: b(&sys, "whisper_auto_insert", true),
        import_max_depth: u(&usr, "import_max_depth", 3),
        import_max_pages: u(&usr, "import_max_pages", 50),
        ai_enable_topics: b(&sys, "ai_enable_topics", true),
        ai_enable_summary: b(&sys, "ai_enable_summary", true),
        ai_enable_vision: b(&sys, "ai_enable_vision", true),
        llm_model_path: s(&sys, "llm_model_path", ""),
        llama_cli_path: s(&sys, "llama_cli_path", ""),
        embedding_model_path: s(&sys, "embedding_model_path", ""),
        last_open_note: s(&usr, "last_open_note", ""),
        onboarding_done: b(&usr, "onboarding_done", false),
        recent_vaults,
        sidebar_width: u(&usr, "sidebar_width", 240),
        graph_panel_width: u(&usr, "graph_panel_width", 320),
        sort_orders: s(&usr, "sort_orders", "{}"),
        font_sans: s(&usr, "font_sans", ""),
        font_mono: s(&usr, "font_mono", ""),
        editor_font_size: u(&usr, "editor_font_size", 14),
        ui_font_size: u(&usr, "ui_font_size", 14),
        graph_font_size: u(&usr, "graph_font_size", 11),
        debug_mode: b(&usr, "debug_mode", false),
        voice_process_mode: s(&usr, "voice_process_mode", "none"),
        voice_preview_enabled: b(&usr, "voice_preview_enabled", true),
        voice_noise_suppression: b(&usr, "voice_noise_suppression", true),
        voice_preview_interval: u(&usr, "voice_preview_interval", 5000),
        enable_chat: b(&usr, "enable_chat", false),
        enable_auto_memory: b(&usr, "enable_auto_memory", false),
        memory_threshold: u(&usr, "memory_threshold", 20),
        write_confirm_mode: s(&usr, "write_confirm_mode", "always"),
        chat_auto_include_note: b(&usr, "chat_auto_include_note", false),
    })
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemSettings {
    pub system_current_vault_path: String,
    pub ai_enable_topics: bool,
    pub ai_enable_summary: bool,
    pub ai_enable_vision: bool,
    pub whisper_cli_path: String,
    pub whisper_model_path: String,
    pub whisper_threads: u32,
    pub whisper_auto_insert: bool,
    pub llm_model_path: String,
    pub llama_cli_path: String,
    pub embedding_model_path: String,
}

#[tauri::command]
pub async fn get_system_settings(state: State<'_, AppState>) -> Result<SystemSettings, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let sys: HashMap<String, String> = daemon_get(&state.http_client, "/settings", tok)
        .await.unwrap_or_default();
    Ok(SystemSettings {
        system_current_vault_path: s(&sys, "system_current_vault_path", ""),
        ai_enable_topics: b(&sys, "ai_enable_topics", true),
        ai_enable_summary: b(&sys, "ai_enable_summary", true),
        ai_enable_vision: b(&sys, "ai_enable_vision", true),
        whisper_cli_path: s(&sys, "whisper_cli_path", ""),
        whisper_model_path: s(&sys, "whisper_model_path", ""),
        whisper_threads: u(&sys, "whisper_threads", 4),
        whisper_auto_insert: b(&sys, "whisper_auto_insert", true),
        llm_model_path: s(&sys, "llm_model_path", ""),
        llama_cli_path: s(&sys, "llama_cli_path", ""),
        embedding_model_path: s(&sys, "embedding_model_path", ""),
    })
}

#[tauri::command]
pub async fn save_system_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: SystemSettings,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let mut map = HashMap::new();
    map.insert("system_current_vault_path", settings.system_current_vault_path.clone());
    map.insert("ai_enable_topics", settings.ai_enable_topics.to_string());
    map.insert("ai_enable_summary", settings.ai_enable_summary.to_string());
    map.insert("ai_enable_vision", settings.ai_enable_vision.to_string());
    map.insert("whisper_cli_path", settings.whisper_cli_path.trim().to_string());
    map.insert("whisper_model_path", settings.whisper_model_path.trim().to_string());
    map.insert("whisper_threads", settings.whisper_threads.to_string());
    map.insert("whisper_auto_insert", settings.whisper_auto_insert.to_string());
    map.insert("llm_model_path", settings.llm_model_path.trim().to_string());
    map.insert("llama_cli_path", settings.llama_cli_path.trim().to_string());
    map.insert("embedding_model_path", settings.embedding_model_path.trim().to_string());

    daemon_post::<_, serde_json::Value>(&state.http_client, "/settings", &map, tok)
        .await.map_err(|e| AppError::Settings(e))?;

    if !settings.system_current_vault_path.is_empty() {
        let state2 = state.inner().clone();
        let new_path = settings.system_current_vault_path;
        tokio::spawn(async move {
            handle_vault_switch(app, state2, new_path).await;
        });
    }

    Ok(())
}

#[tauri::command]
pub async fn save_personal_settings(
    state: State<'_, AppState>,
    username: String,
    settings: Settings,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let recent_json = serde_json::to_string(&settings.recent_vaults)
        .map_err(|e| AppError::Settings(e.to_string()))?;

    let mut map = HashMap::new();
    map.insert("username", username.clone());
    map.insert("theme", settings.theme);
    map.insert("auto_save_mode", settings.auto_save_mode);
    map.insert("auto_save_delay", settings.auto_save_delay.to_string());
    map.insert("import_max_depth", settings.import_max_depth.to_string());
    map.insert("import_max_pages", settings.import_max_pages.to_string());
    map.insert("last_open_note", settings.last_open_note);
    map.insert("onboarding_done", settings.onboarding_done.to_string());
    map.insert("sidebar_width", settings.sidebar_width.to_string());
    map.insert("graph_panel_width", settings.graph_panel_width.to_string());
    map.insert("sort_orders", settings.sort_orders);
    map.insert("font_sans", settings.font_sans);
    map.insert("font_mono", settings.font_mono);
    map.insert("editor_font_size", settings.editor_font_size.to_string());
    map.insert("ui_font_size", settings.ui_font_size.to_string());
    map.insert("graph_font_size", settings.graph_font_size.to_string());
    map.insert("debug_mode", settings.debug_mode.to_string());
    map.insert("voice_process_mode", settings.voice_process_mode);
    map.insert("voice_preview_enabled", settings.voice_preview_enabled.to_string());
    map.insert("voice_noise_suppression", settings.voice_noise_suppression.to_string());
    map.insert("voice_preview_interval", settings.voice_preview_interval.to_string());
    map.insert("enable_chat", settings.enable_chat.to_string());
    map.insert("enable_auto_memory", settings.enable_auto_memory.to_string());
    map.insert("memory_threshold", settings.memory_threshold.to_string());
    map.insert("write_confirm_mode", settings.write_confirm_mode);
    map.insert("chat_auto_include_note", settings.chat_auto_include_note.to_string());
    map.insert("personal_current_vault_path", settings.personal_current_vault_path);
    map.insert("recent_vaults", recent_json);

    daemon_post::<_, serde_json::Value>(&state.http_client, "/settings/user", &map, tok)
        .await.map_err(|e| AppError::Settings(e))?;

    Ok(())
}

pub async fn handle_vault_switch(app: AppHandle, state: AppState, new_path: String) {
    let old_path = state.get_vault_path().await;
    if old_path == new_path {
        state.set_vault_path(new_path).await;
        return;
    }

    // 停止舊 watcher
    { let mut g = state.watcher_stop.lock().await; drop(g.take()); }

    state.set_vault_path(new_path.clone()).await;

    let username = state.get_username().await;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    // 向 daemon 取得 / 建立 vault UUID
    if let Ok(v) = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/vaults",
        &serde_json::json!({"path": new_path, "account": username}),
        tok,
    ).await {
        if let Some(uuid) = v["vault_id"].as_str() {
            state.set_vault_uuid(uuid.to_string()).await;
        }
    }

    let vault_uuid = state.get_vault_uuid().await;
    let path = std::path::PathBuf::from(&new_path);
    if path.exists() {
        // 向 daemon 觸發 scan（背景 chunk + embed + index）
        {
            let client = state.http_client.clone();
            let tok2 = token.clone();
            let vid = vault_uuid.clone();
            tokio::spawn(async move {
                let t = if tok2.is_empty() { None } else { Some(tok2.as_str()) };
                let _ = daemon_post::<_, serde_json::Value>(
                    &client,
                    &format!("/vaults/{}/scan", urlencoding::encode(&vid)),
                    &serde_json::json!({}),
                    t,
                ).await;
            });
        }
        // 確保 memory_agent 排程存在（發給 daemon scheduled_tasks）
        {
            let client = state.http_client.clone();
            let tok2 = token.clone();
            let vid = vault_uuid.clone();
            let aid = state.get_username().await;
            let run_at_ts = chrono::Utc::now().timestamp() + 8 * 3600;
            tokio::spawn(async move {
                let t = if tok2.is_empty() { None } else { Some(tok2.as_str()) };
                let _ = daemon_post::<_, serde_json::Value>(
                    &client,
                    "/scheduled-tasks",
                    &serde_json::json!({
                        "vault_id": vid,
                        "account_id": aid,
                        "description": "Memory Agent",
                        "agent_def_name": "memory_agent",
                        "agent_prompt": "請開始分析並提取記憶。",
                        "run_at_ts": run_at_ts,
                        "repeat_interval_secs": 28800
                    }),
                    t,
                ).await;
            });
        }
        let stop_tx = vault::watcher::start_watcher(app, path);
        *state.watcher_stop.lock().await = Some(stop_tx);
    }

    // seed-builtins 由 service auth 層在 login/register 時 per-account 處理
}

#[tauri::command]
pub async fn get_vault_last_note(
    state: State<'_, AppState>,
    vault_path: String,
) -> Result<Option<String>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let key = format!("vault_last_note_{}", vault_path);
    let res: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/settings/key/{}", urlencoding::encode(&key)),
        tok,
    ).await.unwrap_or_default();
    Ok(res["value"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string()))
}

#[tauri::command]
pub async fn set_vault_last_note(
    state: State<'_, AppState>,
    vault_path: String,
    note_path: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let key = format!("vault_last_note_{}", vault_path);
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/settings",
        &serde_json::json!({key: note_path}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Settings(e))
}

#[tauri::command]
pub async fn get_kb_chat_messages(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<String>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let key = format!("kb_chat_messages:{}", session_id);
    let res: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/settings/key/{}", urlencoding::encode(&key)),
        tok,
    ).await.unwrap_or_default();
    Ok(res["value"].as_str().filter(|s| !s.is_empty() && *s != "[]").map(|s| s.to_string()))
}

#[tauri::command]
pub async fn save_kb_chat_messages(
    state: State<'_, AppState>,
    session_id: String,
    messages_json: String,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let key = format!("kb_chat_messages:{}", session_id);
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/settings",
        &serde_json::json!({key: messages_json}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Settings(e))
}

#[tauri::command]
pub async fn get_last_mode_conversation_id(
    state: State<'_, AppState>,
    username: String,
    mode: String,
) -> Result<Option<String>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let key = format!("last_{}_conversation_id", mode);
    let res: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/settings/user-key/{}?username={}", urlencoding::encode(&key), urlencoding::encode(&username)),
        tok,
    ).await.unwrap_or_default();
    Ok(res["value"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string()))
}

#[tauri::command]
pub async fn set_last_mode_conversation_id(
    state: State<'_, AppState>,
    username: String,
    mode: String,
    conversation_id: Option<String>,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let key = format!("last_{}_conversation_id", mode);
    let val = conversation_id.unwrap_or_default();
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/settings/user",
        &serde_json::json!({"username": username, key: val}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Settings(e))
}

#[tauri::command]
pub async fn get_last_chat_conversation_id(
    state: State<'_, AppState>,
    username: String,
) -> Result<Option<String>, AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let res: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/settings/user-key/last_chat_conversation_id?username={}", urlencoding::encode(&username)),
        tok,
    ).await.unwrap_or_default();
    Ok(res["value"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string()))
}

#[tauri::command]
pub async fn set_last_chat_conversation_id(
    state: State<'_, AppState>,
    username: String,
    conversation_id: Option<String>,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let val = conversation_id.unwrap_or_default();
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/settings/user",
        &serde_json::json!({"username": username, "last_chat_conversation_id": val}),
        tok,
    ).await.map(|_| ()).map_err(|e| AppError::Settings(e))
}

#[tauri::command]
pub async fn get_api_key(
    provider: String,
    state: State<'_, AppState>,
) -> Result<Option<String>, AppError> {
    // 先查記憶體快取
    {
        let cache = state.api_key_cache.lock().await;
        if let Some(key) = cache.get(&provider) {
            return Ok(if key.is_empty() { None } else { Some(key.clone()) });
        }
    }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let db_key = format!("api_key_{}", provider);
    let res: serde_json::Value = daemon_get(
        &state.http_client,
        &format!("/settings/key/{}", urlencoding::encode(&db_key)),
        tok,
    ).await.unwrap_or_default();
    let enc = res["value"].as_str().unwrap_or("").to_string();
    let plain = if enc.is_empty() { String::new() } else { crate::crypto::decrypt_api_key(&enc) };
    let result = if plain.is_empty() { None } else { Some(plain.clone()) };
    state.api_key_cache.lock().await.insert(provider, plain);
    Ok(result)
}

#[tauri::command]
pub async fn set_api_key(
    provider: String,
    key: String,
    state: State<'_, AppState>,
) -> Result<(), AppError> {
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let db_key = format!("api_key_{}", provider);
    let encrypted = crate::crypto::encrypt_api_key(&key);
    daemon_post::<_, serde_json::Value>(
        &state.http_client,
        "/settings",
        &serde_json::json!({db_key: encrypted}),
        tok,
    ).await.map_err(|e| AppError::Settings(e))?;
    state.api_key_cache.lock().await.insert(provider, key);
    Ok(())
}

#[tauri::command]
pub fn check_vcredist() -> bool {
    #[cfg(target_os = "windows")]
    {
        use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        hklm.open_subkey("SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x64")
            .and_then(|key| key.get_value::<u32, _>("Installed"))
            .map(|v: u32| v == 1)
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    { true }
}
