mod commands;
mod db;
mod error;
pub mod runtime;
mod state;
pub mod tools;
mod vault;

use commands::auth::{login, logout, get_session, change_password, start_google_oauth};
use commands::{
    ai::{stream_chat_external, process_with_llm, stop_llama_server, warmup_llama_server,
         get_llama_server_status, start_llama_server, restart_llama_server,
         save_memory_session, query_memory, add_memory_rule,
         get_memory_rules, delete_memory_rule, confirm_write_tool,
         test_vault_tool, run_tool_pipeline, cancel_tool_test, cancel_agent, invoke_agent,
         get_intent_keywords, save_intent_keywords, delete_intent_row},
    conversation::{create_conversation, list_conversations, get_conversation,
                   delete_conversation, update_conversation_title, save_conversation_messages},
    download::*, graph::*, import::*, search::*,
    knowledge_import::{create_import_session, list_import_sessions, delete_import_session,
                       fetch_site_outline, import_page, check_page_updates, get_session_pages,
                       query_knowledge},
    settings::{get_settings, save_personal_settings, get_system_settings, save_system_settings, get_api_key, set_api_key,
               get_vault_last_note, set_vault_last_note, check_vcredist,
               get_last_chat_conversation_id, set_last_chat_conversation_id,
               get_last_mode_conversation_id, set_last_mode_conversation_id},
    vault::*,
    voice::{transcribe_audio, stop_whisper_server, warmup_whisper_server,
            get_whisper_server_status, start_whisper_server, restart_whisper_server},
};
use state::AppState;
use std::time::Duration;
use tauri::{
    generate_handler,
    Manager,
};

#[tauri::command]
fn open_devtools(window: tauri::WebviewWindow) {
    window.open_devtools();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_log::Builder::new()
            .level(log::LevelFilter::Info)
            .build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // 管理下載狀態（在 block_on 之前，避免所有權問題）
            app.manage(commands::download::DownloadState::new());

            let app_handle = app.handle().clone();

            // 同步完成最小必要初始化：SurrealDB 初始化 + manage state
            let state = tauri::async_runtime::block_on(async {
                let app_data_dir = app_handle
                    .path()
                    .app_data_dir()
                    .expect("無法取得 app data 目錄");

                let surreal_db = db::surreal::init_db(&app_data_dir)
                    .await
                    .expect("SurrealDB 初始化失敗");

                AppState::new(surreal_db)
            });

            app_handle.manage(state);

            // Windows：移除原生 title bar，改用自訂 TitleBar 元件
            #[cfg(target_os = "windows")]
            if let Some(window) = app_handle.get_webview_window("main") {
                let _ = window.set_decorations(false);
            }

            // 背景完成 vault 初始化、FileWatcher、server warmup
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();

                // 載入已設定的 vault 路徑
                if let Ok(Some(vp)) = db::queries::get_setting(&state.db, "system_current_vault_path").await {
                    if !vp.is_empty() {
                        state.set_vault_path(vp.clone()).await;
                        let path = std::path::PathBuf::from(&vp);
                        if path.exists() {
                            // 背景補齊 chunk 索引（不阻塞啟動）
                            {
                                let db = state.db.clone();
                                let vid = vp.clone();
                                tokio::spawn(async move {
                                    let _ = vault::chunker::reindex_all(&db, &vid).await;
                                });
                            }
                            let stop_tx = vault::watcher::start_watcher(app_handle.clone(), path);
                            *state.watcher_stop.lock().await = Some(stop_tx);
                        }
                    }
                }

                // 預熱 whisper-server 與 llama-server（延遲 2 秒等前端 register 監聽器）
                tokio::time::sleep(Duration::from_secs(2)).await;
                tokio::join!(
                    warmup_whisper_server(&state, &app_handle),
                    warmup_llama_server(&state, &app_handle),
                );
            });

            Ok(())
        })
        .register_uri_scheme_protocol("vault", |ctx, request| {
            handle_vault_protocol(ctx.app_handle(), request)
        })
        .invoke_handler(generate_handler![
            // Auth
            login,
            logout,
            get_session,
            change_password,
            start_google_oauth,
            // Settings
            get_settings,
            save_personal_settings,
            get_system_settings,
            save_system_settings,
            get_api_key,
            set_api_key,
            get_vault_last_note,
            set_vault_last_note,
            check_vcredist,
            get_last_chat_conversation_id,
            set_last_chat_conversation_id,
            get_last_mode_conversation_id,
            set_last_mode_conversation_id,
            // Vault
            create_note,
            read_note,
            update_note,
            delete_note,
            rename_note,
            rename_folder,
            list_notes,
            get_backlinks,
            scan_vault,
            move_note,
            move_folder,
            read_file_base64,
            read_vault_file_base64,
            create_folder,
            list_folders,
            delete_folder,
            import_image,
            list_assets,
            download_asset_to_vault,
            delete_asset,
            rename_asset,
            open_path_externally,
            // Trash
            trash_note,
            trash_folder,
            list_trash,
            restore_trash_item,
            delete_trash_items,
            // Search / Chunks
            search,
            reindex_vault_chunks,
            search_vault_chunks,
            // Graph
            get_graph,
            // Import
            import_url,
            // Knowledge Import
            create_import_session,
            list_import_sessions,
            delete_import_session,
            fetch_site_outline,
            import_page,
            check_page_updates,
            get_session_pages,
            query_knowledge,
            // Voice
            transcribe_audio,
            stop_whisper_server,
            get_whisper_server_status,
            start_whisper_server,
            restart_whisper_server,
            // AI / LLM
            stream_chat_external,
            process_with_llm,
            confirm_write_tool,
            test_vault_tool,
            run_tool_pipeline,
            cancel_tool_test,
            stop_llama_server,
            get_llama_server_status,
            start_llama_server,
            restart_llama_server,
            save_memory_session,
            query_memory,
            add_memory_rule,
            get_memory_rules,
            delete_memory_rule,
            cancel_agent,
            invoke_agent,
            // Conversation management
            create_conversation,
            list_conversations,
            get_conversation,
            delete_conversation,
            update_conversation_title,
            save_conversation_messages,
            get_intent_keywords,
            save_intent_keywords,
            delete_intent_row,
            // Download
            get_models_dir,
            get_downloaded_models,
            start_model_download,
            cancel_model_download,
            delete_model_file,
            get_external_model_paths,
            set_external_model_paths,
            import_model_file,
            get_whisper_binary_path,
            download_whisper_server,
            get_llama_binary_path,
            download_llama_server,
            get_coreml_model_path,
            download_coreml_model,
            // DevTools (debug helper)
            open_devtools,
        ])
        .build(tauri::generate_context!())
        .expect("noteTreeLM 構建失敗")
        .run(|app_handle, event| {
            // App 結束時自動 kill llama-server 與 whisper-server
            if let tauri::RunEvent::Exit = event {
                if let Some(state) = app_handle.try_state::<AppState>() {
                    tauri::async_runtime::block_on(async {
                        let mut llama_guard = state.llama_server.lock().await;
                        if let Some(mut child) = llama_guard.take() {
                            let _ = child.kill().await;
                        }
                        let mut whisper_guard = state.whisper_server.lock().await;
                        if let Some(mut child) = whisper_guard.take() {
                            let _ = child.kill().await;
                        }
                    });
                }
            }
        });
}

/// vault:// 自訂協定：提供 Vault 內的圖片和資源
fn handle_vault_protocol(
    app: &tauri::AppHandle,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::Response;

    let state = match app.try_state::<AppState>() {
        Some(s) => s,
        None => {
            return Response::builder()
                .status(503)
                .body(b"App state not ready".to_vec())
                .unwrap();
        }
    };

    let vault_path = tauri::async_runtime::block_on(state.get_vault_path());
    if vault_path.is_empty() {
        return Response::builder()
            .status(404)
            .body(b"Vault not configured".to_vec())
            .unwrap();
    }

    // 從 URI 解析檔案路徑
    let uri = request.uri().path();
    let rel_path = uri.trim_start_matches('/');

    // 安全性：防止路徑穿越
    if rel_path.contains("..") {
        return Response::builder()
            .status(403)
            .body(b"Forbidden".to_vec())
            .unwrap();
    }

    let file_path = std::path::PathBuf::from(&vault_path).join(rel_path);

    match std::fs::read(&file_path) {
        Ok(data) => {
            let mime = mime_guess::from_path(&file_path)
                .first_or_octet_stream()
                .to_string();
            Response::builder()
                .status(200)
                .header("Content-Type", mime)
                .header("Access-Control-Allow-Origin", "*")
                .body(data)
                .unwrap()
        }
        Err(_) => Response::builder()
            .status(404)
            .body(b"File not found".to_vec())
            .unwrap(),
    }
}
