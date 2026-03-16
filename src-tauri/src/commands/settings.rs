use crate::{db::queries, error::AppError, state::AppState, vault};
use serde::{Deserialize, Serialize};
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
    pub whisper_language: String,
    pub whisper_threads: u32,
    pub whisper_auto_insert: bool,
    pub import_max_depth: u32,
    pub import_max_pages: u32,
    pub ai_provider: String,
    pub ai_model: String,
    pub ai_base_url: String,
    pub ai_enable_topics: bool,
    pub ai_enable_summary: bool,
    pub ai_enable_vision: bool,
    pub llm_model_path: String,
    pub llama_cli_path: String,
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

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>, username: String) -> Result<Settings, AppError> {
    let pool = &state.settings_db;

    // 個人設定：只查 user_settings，不 fallback
    macro_rules! get {
        ($key:expr, $default:expr) => {
            queries::get_user_setting(pool, &username, $key)
                .await?
                .unwrap_or_else(|| $default.to_string())
        };
    }

    // 系統設定：只查全域 settings
    macro_rules! gget {
        ($key:expr, $default:expr) => {
            queries::get_setting(pool, $key)
                .await?
                .unwrap_or_else(|| $default.to_string())
        };
    }

    let recent_vaults_json = get!("recent_vaults", "[]");
    let recent_vaults: Vec<String> =
        serde_json::from_str(&recent_vaults_json).unwrap_or_default();

    Ok(Settings {
        system_current_vault_path: gget!("system_current_vault_path", ""),
        personal_current_vault_path: get!("personal_current_vault_path", ""),
        theme: get!("theme", "dark"),
        auto_save_mode: get!("auto_save_mode", "afterDelay"),
        auto_save_delay: get!("auto_save_delay", "1000").parse().unwrap_or(1000),
        // System keys — read from global settings only
        whisper_cli_path: gget!("whisper_cli_path", ""),
        whisper_model_path: gget!("whisper_model_path", ""),
        whisper_language: gget!("whisper_language", "auto"),
        whisper_threads: gget!("whisper_threads", "4").parse().unwrap_or(4),
        whisper_auto_insert: gget!("whisper_auto_insert", "true") == "true",
        import_max_depth: get!("import_max_depth", "3").parse().unwrap_or(3),
        import_max_pages: get!("import_max_pages", "50").parse().unwrap_or(50),
        ai_provider: gget!("ai_provider", ""),
        ai_model: gget!("ai_model", "gpt-4o"),
        ai_base_url: gget!("ai_base_url", "https://api.openai.com/v1"),
        ai_enable_topics: gget!("ai_enable_topics", "true") == "true",
        ai_enable_summary: gget!("ai_enable_summary", "true") == "true",
        ai_enable_vision: gget!("ai_enable_vision", "true") == "true",
        llm_model_path: gget!("llm_model_path", ""),
        llama_cli_path: gget!("llama_cli_path", ""),
        // Personal keys — read from user_settings
        last_open_note: get!("last_open_note", ""),
        onboarding_done: get!("onboarding_done", "false") == "true",
        recent_vaults,
        sidebar_width: get!("sidebar_width", "240").parse().unwrap_or(240),
        graph_panel_width: get!("graph_panel_width", "320").parse().unwrap_or(320),
        sort_orders: get!("sort_orders", "{}"),
        font_sans: get!("font_sans", ""),
        font_mono: get!("font_mono", ""),
        editor_font_size: get!("editor_font_size", "14").parse().unwrap_or(14),
        ui_font_size: get!("ui_font_size", "14").parse().unwrap_or(14),
        graph_font_size: get!("graph_font_size", "11").parse().unwrap_or(11),
        debug_mode: get!("debug_mode", "false") == "true",
        voice_process_mode: get!("voice_process_mode", "none"),
        voice_preview_enabled: get!("voice_preview_enabled", "true") == "true",
        voice_noise_suppression: get!("voice_noise_suppression", "true") == "true",
        voice_preview_interval: get!("voice_preview_interval", "5000").parse().unwrap_or(5000),
        enable_chat: get!("enable_chat", "false") == "true",
        enable_auto_memory: get!("enable_auto_memory", "false") == "true",
        memory_threshold: get!("memory_threshold", "20").parse().unwrap_or(20),
        write_confirm_mode: get!("write_confirm_mode", "always"),
        chat_auto_include_note: get!("chat_auto_include_note", "false") == "true",
    })
}

#[tauri::command]
pub async fn get_system_settings(state: State<'_, AppState>) -> Result<SystemSettings, AppError> {
    let pool = &state.settings_db;

    macro_rules! gget {
        ($key:expr, $default:expr) => {
            queries::get_setting(pool, $key)
                .await?
                .unwrap_or_else(|| $default.to_string())
        };
    }

    Ok(SystemSettings {
        system_current_vault_path: gget!("system_current_vault_path", ""),
        ai_provider: gget!("ai_provider", ""),
        ai_model: gget!("ai_model", "gpt-4o"),
        ai_base_url: gget!("ai_base_url", "https://api.openai.com/v1"),
        ai_enable_topics: gget!("ai_enable_topics", "true") == "true",
        ai_enable_summary: gget!("ai_enable_summary", "true") == "true",
        ai_enable_vision: gget!("ai_enable_vision", "true") == "true",
        whisper_cli_path: gget!("whisper_cli_path", ""),
        whisper_model_path: gget!("whisper_model_path", ""),
        whisper_language: gget!("whisper_language", "auto"),
        whisper_threads: gget!("whisper_threads", "4").parse().unwrap_or(4),
        whisper_auto_insert: gget!("whisper_auto_insert", "true") == "true",
        llm_model_path: gget!("llm_model_path", ""),
        llama_cli_path: gget!("llama_cli_path", ""),
    })
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemSettings {
    pub system_current_vault_path: String,
    pub ai_provider: String,
    pub ai_model: String,
    pub ai_base_url: String,
    pub ai_enable_topics: bool,
    pub ai_enable_summary: bool,
    pub ai_enable_vision: bool,
    pub whisper_cli_path: String,
    pub whisper_model_path: String,
    pub whisper_language: String,
    pub whisper_threads: u32,
    pub whisper_auto_insert: bool,
    pub llm_model_path: String,
    pub llama_cli_path: String,
}

#[tauri::command]
pub async fn save_system_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    settings: SystemSettings,
) -> Result<(), AppError> {
    let pool = &state.settings_db;

    macro_rules! gsave {
        ($key:expr, $value:expr) => {
            queries::set_setting(pool, $key, &$value.to_string()).await?
        };
    }

    gsave!("system_current_vault_path", settings.system_current_vault_path);
    gsave!("ai_provider", settings.ai_provider);
    gsave!("ai_model", settings.ai_model);
    gsave!("ai_base_url", settings.ai_base_url);
    gsave!("ai_enable_topics", settings.ai_enable_topics);
    gsave!("ai_enable_summary", settings.ai_enable_summary);
    gsave!("ai_enable_vision", settings.ai_enable_vision);
    gsave!("whisper_cli_path", settings.whisper_cli_path.trim());
    gsave!("whisper_model_path", settings.whisper_model_path.trim());
    gsave!("whisper_language", settings.whisper_language);
    gsave!("whisper_threads", settings.whisper_threads);
    gsave!("whisper_auto_insert", settings.whisper_auto_insert);
    gsave!("llm_model_path", settings.llm_model_path.trim());
    gsave!("llama_cli_path", settings.llama_cli_path.trim());

    // 切換 vault（非空時）
    if !settings.system_current_vault_path.is_empty() {
        handle_vault_switch(app, state.inner().clone(), settings.system_current_vault_path).await;
    }

    Ok(())
}

#[tauri::command]
pub async fn save_personal_settings(
    state: State<'_, AppState>,
    username: String,
    settings: Settings,
) -> Result<(), AppError> {
    let pool = state.settings_db.clone();

    macro_rules! save {
        ($key:expr, $value:expr) => {
            queries::set_user_setting(&pool, &username, $key, &$value.to_string()).await?
        };
    }

    // vault_path is a system setting — managed by save_system_settings only.
    save!("theme", settings.theme);
    save!("auto_save_mode", settings.auto_save_mode);
    save!("auto_save_delay", settings.auto_save_delay);
    // System keys (ai_*, whisper_*, llm_model_path, llama_cli_path) are managed exclusively
    // by save_system_settings → global settings table only. Never written here.
    save!("import_max_depth", settings.import_max_depth);
    save!("import_max_pages", settings.import_max_pages);
    save!("last_open_note", settings.last_open_note);
    save!("onboarding_done", settings.onboarding_done);
    save!("sidebar_width", settings.sidebar_width);
    save!("graph_panel_width", settings.graph_panel_width);
    save!("sort_orders", settings.sort_orders);
    save!("font_sans", settings.font_sans);
    save!("font_mono", settings.font_mono);
    save!("editor_font_size", settings.editor_font_size);
    save!("ui_font_size", settings.ui_font_size);
    save!("graph_font_size", settings.graph_font_size);
    save!("debug_mode", settings.debug_mode);
    save!("voice_process_mode", settings.voice_process_mode);
    save!("voice_preview_enabled", settings.voice_preview_enabled);
    save!("voice_noise_suppression", settings.voice_noise_suppression);
    save!("voice_preview_interval", settings.voice_preview_interval);
    save!("enable_chat", settings.enable_chat);
    save!("enable_auto_memory", settings.enable_auto_memory);
    save!("memory_threshold", settings.memory_threshold);
    save!("write_confirm_mode", settings.write_confirm_mode);
    save!("chat_auto_include_note", settings.chat_auto_include_note);
    save!("personal_current_vault_path", settings.personal_current_vault_path);

    let recent_json = serde_json::to_string(&settings.recent_vaults)
        .map_err(|e| AppError::Settings(e.to_string()))?;
    queries::set_user_setting(&pool, &username, "recent_vaults", &recent_json).await?;

    Ok(())
}

/// 處理 vault 切換：關閉舊 DB、初始化新 DB、重啟 FileWatcher
/// 獨立函式讓 AppState 以 clone（Arc-cheap）方式傳入，避免 `State<'_, AppState>` 的 HRTB Send 問題
async fn handle_vault_switch(app: AppHandle, state: AppState, new_path: String) {
    let old_path = state.get_vault_path().await;

    if old_path != new_path {
        // 停止舊 watcher（drop sender 即停止 thread）
        {
            let mut guard = state.watcher_stop.lock().await;
            drop(guard.take());
        }
        // 關閉舊 vault DB
        state.set_vault_db(None).await;
        // 更新 vault_path
        state.set_vault_path(new_path.clone()).await;
        // 初始化新 vault DB 並啟動新 watcher（若路徑有效）
        let path = std::path::PathBuf::from(&new_path);
        if path.exists() {
            if let Ok(vault_pool) = crate::db::init_vault_db(&path).await {
                // 背景補齊 chunk 索引
                {
                    let db = vault_pool.clone();
                    let notes_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes")
                        .fetch_one(&db).await.unwrap_or(0);
                    let chunks_count: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT file_path) FROM chunks")
                        .fetch_one(&db).await.unwrap_or(0);
                    if chunks_count < notes_count {
                        tokio::spawn(async move {
                            let _ = crate::vault::chunker::reindex_all(&db).await;
                        });
                    }
                }
                state.set_vault_db(Some(vault_pool)).await;
            }
            let stop_tx = vault::watcher::start_watcher(app, path);
            *state.watcher_stop.lock().await = Some(stop_tx);
        }
    } else {
        state.set_vault_path(new_path).await;
    }
}

#[tauri::command]
pub async fn get_vault_last_note(
    state: State<'_, AppState>,
    vault_path: String,
) -> Result<Option<String>, AppError> {
    queries::get_vault_last_note(&state.settings_db, &vault_path).await
}

#[tauri::command]
pub async fn set_vault_last_note(
    state: State<'_, AppState>,
    vault_path: String,
    note_path: String,
) -> Result<(), AppError> {
    queries::set_vault_last_note(&state.settings_db, &vault_path, &note_path).await
}

#[tauri::command]
pub async fn get_last_chat_conversation_id(
    state: State<'_, AppState>,
    username: String,
) -> Result<Option<String>, AppError> {
    queries::get_user_setting(&state.settings_db, &username, "last_chat_conversation_id").await
}

#[tauri::command]
pub async fn set_last_chat_conversation_id(
    state: State<'_, AppState>,
    username: String,
    conversation_id: Option<String>,
) -> Result<(), AppError> {
    match conversation_id {
        Some(id) if !id.is_empty() => {
            queries::set_user_setting(&state.settings_db, &username, "last_chat_conversation_id", &id).await
        }
        _ => {
            queries::set_user_setting(&state.settings_db, &username, "last_chat_conversation_id", "").await
        }
    }
}

#[tauri::command]
pub async fn get_api_key(provider: String) -> Result<Option<String>, AppError> {
    let entry = keyring::Entry::new("com.notetreelm.app", &provider)
        .map_err(|e| AppError::Settings(e.to_string()))?;
    match entry.get_password() {
        Ok(key) => Ok(Some(key)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(AppError::Settings(e.to_string())),
    }
}

#[tauri::command]
pub async fn set_api_key(provider: String, key: String) -> Result<(), AppError> {
    let entry = keyring::Entry::new("com.notetreelm.app", &provider)
        .map_err(|e| AppError::Settings(e.to_string()))?;
    if key.is_empty() {
        entry.delete_password().ok();
    } else {
        entry.set_password(&key)
            .map_err(|e| AppError::Settings(e.to_string()))?;
    }
    Ok(())
}

/// 檢查 Windows 是否已安裝 Visual C++ Redistributable 2015-2022 (x64)。
/// 非 Windows 平台永遠回傳 true（不需要）。
#[tauri::command]
pub fn check_vcredist() -> bool {
    #[cfg(target_os = "windows")]
    {
        // 直接讀 Registry，避免啟動子 process（reg.exe 啟動 + AV 掃描很慢）
        use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};
        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        hklm.open_subkey(
            "SOFTWARE\\Microsoft\\VisualStudio\\14.0\\VC\\Runtimes\\x64"
        )
        .and_then(|key| key.get_value::<u32, _>("Installed"))
        .map(|v: u32| v == 1)
        .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}
