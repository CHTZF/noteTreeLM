use crate::{db::queries, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Settings {
    pub vault_path: String,
    pub theme: String,
    pub auto_save_mode: String,
    pub auto_save_delay: u32,
    pub whisper_cli_path: String,
    pub whisper_model_path: String,
    pub whisper_language: String,
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
    pub enable_chat: bool,
    pub llama_server_port: u32,
    pub whisper_server_port: u32,
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<Settings, AppError> {
    let pool = &state.db;

    macro_rules! get {
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
        vault_path: get!("vault_path", ""),
        theme: get!("theme", "dark"),
        auto_save_mode: get!("auto_save_mode", "afterDelay"),
        auto_save_delay: get!("auto_save_delay", "1000").parse().unwrap_or(1000),
        whisper_cli_path: get!("whisper_cli_path", ""),
        whisper_model_path: get!("whisper_model_path", ""),
        whisper_language: get!("whisper_language", "auto"),
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
        enable_chat: get!("enable_chat", "false") == "true",
        llama_server_port: get!("llama_server_port", "8080").parse().unwrap_or(8080),
        whisper_server_port: get!("whisper_server_port", "8081").parse().unwrap_or(8081),
    })
}

#[tauri::command]
pub async fn save_settings(
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), AppError> {
    let pool = &state.db;

    macro_rules! save {
        ($key:expr, $value:expr) => {
            queries::set_setting(pool, $key, &$value.to_string()).await?
        };
    }

    save!("vault_path", settings.vault_path);
    save!("theme", settings.theme);
    save!("auto_save_mode", settings.auto_save_mode);
    save!("auto_save_delay", settings.auto_save_delay);
    save!("whisper_cli_path", settings.whisper_cli_path);
    save!("whisper_model_path", settings.whisper_model_path);
    save!("whisper_language", settings.whisper_language);
    save!("whisper_auto_insert", settings.whisper_auto_insert);
    save!("import_max_depth", settings.import_max_depth);
    save!("import_max_pages", settings.import_max_pages);
    save!("ai_provider", settings.ai_provider);
    save!("ai_model", settings.ai_model);
    save!("ai_base_url", settings.ai_base_url);
    save!("ai_enable_topics", settings.ai_enable_topics);
    save!("ai_enable_summary", settings.ai_enable_summary);
    save!("ai_enable_vision", settings.ai_enable_vision);
    save!("llm_model_path", settings.llm_model_path);
    save!("llama_cli_path", settings.llama_cli_path);
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
    save!("enable_chat", settings.enable_chat);
    save!("llama_server_port", settings.llama_server_port);
    save!("whisper_server_port", settings.whisper_server_port);

    let recent_json = serde_json::to_string(&settings.recent_vaults)
        .map_err(|e| AppError::Settings(e.to_string()))?;
    queries::set_setting(pool, "recent_vaults", &recent_json).await?;

    // 更新記憶體中的 vault_path
    if !settings.vault_path.is_empty() {
        state.set_vault_path(settings.vault_path).await;
    }

    Ok(())
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
