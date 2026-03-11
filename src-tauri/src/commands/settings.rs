use crate::{db::queries, error::AppError, state::AppState, vault};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub vault_path: String,
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

    macro_rules! get {
        ($key:expr, $default:expr) => {
            queries::get_user_setting(pool, &username, $key)
                .await?
                .unwrap_or_else(|| $default.to_string())
        };
    }

    let recent_vaults_json = get!("recent_vaults", "[]");
    let recent_vaults: Vec<String> =
        serde_json::from_str(&recent_vaults_json).unwrap_or_default();

    Ok(Settings {
        vault_path: get!("vault_path", ""),
        theme: get!("theme", "dark"),
        auto_save_mode: get!("auto_save_mode", "afterDelay"),
        auto_save_delay: get!("auto_save_delay", "1000").parse().unwrap_or(1000),
        whisper_cli_path: get!("whisper_cli_path", ""),
        whisper_model_path: get!("whisper_model_path", ""),
        whisper_language: get!("whisper_language", "auto"),
        whisper_threads: get!("whisper_threads", "4").parse().unwrap_or(4),
        whisper_auto_insert: get!("whisper_auto_insert", "true") == "true",
        import_max_depth: get!("import_max_depth", "3").parse().unwrap_or(3),
        import_max_pages: get!("import_max_pages", "50").parse().unwrap_or(50),
        ai_provider: get!("ai_provider", ""),
        ai_model: get!("ai_model", "gpt-4o"),
        ai_base_url: get!("ai_base_url", "https://api.openai.com/v1"),
        ai_enable_topics: get!("ai_enable_topics", "true") == "true",
        ai_enable_summary: get!("ai_enable_summary", "true") == "true",
        ai_enable_vision: get!("ai_enable_vision", "true") == "true",
        llm_model_path: get!("llm_model_path", ""),
        llama_cli_path: get!("llama_cli_path", ""),
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
pub async fn save_settings(
    app: AppHandle,
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

    save!("vault_path", settings.vault_path);
    save!("theme", settings.theme);
    save!("auto_save_mode", settings.auto_save_mode);
    save!("auto_save_delay", settings.auto_save_delay);
    // Binary paths and model paths are machine-level settings shared across users on the same machine.
    // Store in the global `settings` table so server startup code (which uses get_setting) can read them.
    queries::set_setting(&pool, "whisper_cli_path", settings.whisper_cli_path.trim()).await?;
    queries::set_setting(&pool, "whisper_model_path", settings.whisper_model_path.trim()).await?;
    save!("whisper_language", settings.whisper_language);
    save!("whisper_threads", settings.whisper_threads);
    save!("whisper_auto_insert", settings.whisper_auto_insert);
    save!("import_max_depth", settings.import_max_depth);
    save!("import_max_pages", settings.import_max_pages);
    save!("ai_provider", settings.ai_provider);
    save!("ai_model", settings.ai_model);
    save!("ai_base_url", settings.ai_base_url);
    save!("ai_enable_topics", settings.ai_enable_topics);
    save!("ai_enable_summary", settings.ai_enable_summary);
    save!("ai_enable_vision", settings.ai_enable_vision);
    queries::set_setting(&pool, "llm_model_path", settings.llm_model_path.trim()).await?;
    queries::set_setting(&pool, "llama_cli_path", settings.llama_cli_path.trim()).await?;
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

    let recent_json = serde_json::to_string(&settings.recent_vaults)
        .map_err(|e| AppError::Settings(e.to_string()))?;
    queries::set_user_setting(&pool, &username, "recent_vaults", &recent_json).await?;

    // 更新記憶體中的 vault_path，並在切換 vault 時重啟 FileWatcher
    if !settings.vault_path.is_empty() {
        let app_state = state.inner().clone();
        let new_path = settings.vault_path;
        handle_vault_switch(app, app_state, new_path).await;
    }

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
