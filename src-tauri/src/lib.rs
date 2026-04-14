#![recursion_limit = "512"]
mod api_client;
mod commands;
pub mod crypto;
mod error;
pub mod runtime;
mod state;
mod vault;

use commands::auth::{login, logout, get_session, restore_session, change_password, start_google_oauth,
                     connect_google_calendar, get_calendar_status, disconnect_google_calendar,
                     connect_gmail, get_gmail_status, disconnect_gmail};
use commands::{
    agent::{process_with_llm, stop_llama_server, warmup_llama_server,
         get_llama_server_status, start_llama_server, restart_llama_server,
         warmup_embedding_server, get_embedding_server_status, start_embedding_server,
         stop_embedding_server, restart_embedding_server, check_embedding_endpoint,
         query_memory, rate_response, get_conversation_ratings, confirm_write_tool,
         test_vault_tool, run_tool_pipeline, cancel_tool_test, cancel_agent, invoke_agent, invoke_live_chat,
         set_note_status,
         add_skill_trigger},
    conversation::{create_conversation, list_conversations, get_conversation,
                   delete_conversation, update_conversation_title,
                   get_or_create_live_chat_conversation},
    download::*, graph::*, import::*, search::*,
    knowledge_import::{create_import_session, list_import_sessions, delete_import_session,
                       fetch_site_outline, import_page, check_page_updates, get_session_pages,
                       query_knowledge, query_kb, set_session_auto_update, debug_kb_chunks,
                       suggest_kb_cards, get_kb_stats, get_aging_notes, mark_note_reviewed,
                       list_kb_suggestions, dismiss_kb_suggestion, create_kb_card_note,
                       get_brave_search_usage, sync_brave_key_id,
                       save_knowledge_item, list_knowledge_items, get_knowledge_item,
                       delete_knowledge_item, rename_knowledge_item, suggest_kb_cards_for_item,
                       suggest_note_cards_for_item, suggest_skill_cards_for_item,
                       get_cached_page,
                       save_agent_skill, list_agent_skills, toggle_agent_skill, delete_agent_skill,
                       update_agent_skill, compress_conversation_to_knowledge,
                       get_skill_usage_stats, extract_skill_from_exchange},
    knowledge_import::auto_check_all_sessions,
    agent_def::{list_agent_definitions, save_agent_definition, update_agent_definition,
                delete_agent_definition, toggle_agent_definition, wake_agent_definition},
    settings::{get_settings, save_personal_settings, get_system_settings, save_system_settings, get_api_key, set_api_key,
               get_vault_last_note, set_vault_last_note, check_vcredist,
               get_last_chat_conversation_id, set_last_chat_conversation_id,
               get_last_mode_conversation_id, set_last_mode_conversation_id,
               get_kb_chat_messages, save_kb_chat_messages},
    patterns::{save_pattern, update_pattern_score, list_patterns, decay_patterns, set_pattern_intent},
    vault::*,
    voice::{transcribe_audio, stop_whisper_server, warmup_whisper_server,
            get_whisper_server_status, start_whisper_server, restart_whisper_server},
};
use state::AppState;
use std::time::Duration;
use tauri::{
    generate_handler,
    Emitter,
    Manager,
};

#[tauri::command]
fn open_devtools(window: tauri::WebviewWindow) {
    window.open_devtools();
}

#[tauri::command]
fn is_app_ready(state: tauri::State<AppState>) -> bool {
    state.app_ready.load(std::sync::atomic::Ordering::Relaxed)
}

/// Subscribe to the notetreelm-service SSE stream and re-emit each event as a Tauri event.
/// Reconnects automatically on disconnect (up to every 5 seconds).
async fn subscribe_service_events(app_handle: tauri::AppHandle) {
    use futures_util::StreamExt;

    loop {
        let client = match reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
        {
            Ok(c) => c,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let resp = client
            .get("http://127.0.0.1:7787/api/v1/events")
            .header("Accept", "text/event-stream")
            .send()
            .await;

        let response = match resp {
            Ok(r) if r.status().is_success() => r,
            _ => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut stream = response.bytes_stream();
        let mut event_name = String::new();
        let mut data_buf = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(_) => break,
            };
            let text = match std::str::from_utf8(&bytes) {
                Ok(t) => t.to_string(),
                Err(_) => break,
            };

            // Parse SSE lines
            for line in text.lines() {
                if line.starts_with("event:") {
                    event_name = line["event:".len()..].trim().to_string();
                } else if line.starts_with("data:") {
                    data_buf = line["data:".len()..].trim().to_string();
                } else if line.is_empty() && !event_name.is_empty() {
                    // Passthrough events re-emitted without "service:" prefix so
                    // the frontend receives the same event names as the Tauri-native flow.
                    const PASSTHROUGH: &[&str] = &[
                        "llm:token", "llm:done", "llm:think_token",
                        "agent:tool_call", "agent:write_request", "agent:note_refs",
                        "agent:refs", "agent:web_refs",
                        "agent:think", "agent:skills_activated", "agent:plan_announce",
                        "agent:open_note", "agent:skill_suggestion",
                        "agent:citation", "agent:citation_missing", "agent:cite_correction_start",
                        "agent:clear_stream", "agent:cite_status", "agent:skill_found",
                        "agent:skill_not_found", "agent:write_timeout",
                        "memory:prefetched",
                        "whisper:stderr",
                        "llm:stderr",
                    ];
                    let payload: serde_json::Value = serde_json::from_str(&data_buf)
                        .unwrap_or(serde_json::json!({}));
                    if PASSTHROUGH.contains(&event_name.as_str()) {
                        let _ = app_handle.emit(&event_name, payload);
                    } else {
                        let _ = app_handle.emit(&format!("service:{}", event_name), payload);
                    }
                    event_name.clear();
                    data_buf.clear();
                }
            }
        }

        // Connection dropped — reconnect after 5s
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
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

            // 設定 panic hook：偵測 SurrealDB BM25 B-tree 損壞
            // 寫入 db_corruption_detected（供下次啟動時通知使用者），不自動修復
            {
                let data_dir = app_handle
                    .path()
                    .app_data_dir()
                    .expect("無法取得 app data 目錄");
                let flag_path = data_dir.join("db_corruption_detected");
                let prev_hook = std::panic::take_hook();
                std::panic::set_hook(Box::new(move |info| {
                    let msg = info.to_string();
                    if msg.contains("Duplicate insert key") || msg.contains("btree") {
                        let reason = format!(
                            "SurrealDB BM25 B-tree 損壞（{}）",
                            msg.lines().next().unwrap_or("Duplicate insert key panic")
                        );
                        let _ = std::fs::write(&flag_path, &reason);
                    }
                    prev_hook(info);
                }));
            }

            // 同步完成最小必要初始化：SurrealDB 初始化 + manage state
            // 同時讀取損壞通知 flag（若有）
            let (state, corruption_msg) = tauri::async_runtime::block_on(async {
                let app_data_dir = app_handle
                    .path()
                    .app_data_dir()
                    .expect("無法取得 app data 目錄");

                // 讀取並清除損壞 flag
                let flag = app_data_dir.join("db_corruption_detected");
                let corruption = if flag.exists() {
                    let msg = tokio::fs::read_to_string(&flag).await.ok();
                    let _ = tokio::fs::remove_file(&flag).await;
                    msg
                } else {
                    None
                };

                (AppState::new(), corruption)
            });

            app_handle.manage(state);

            // 若偵測到損壞，延遲 1.5s（等前端 ready）後 emit 通知
            if let Some(reason) = corruption_msg {
                let ah = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    let _ = ah.emit("db:corruption_detected", serde_json::json!({ "reason": reason }));
                });
            }

            // Windows：移除原生 title bar，改用自訂 TitleBar 元件
            #[cfg(target_os = "windows")]
            if let Some(window) = app_handle.get_webview_window("main") {
                let _ = window.set_decorations(false);
            }

            // 攔截 Ctrl+C / SIGINT：走 Tauri graceful exit
            // llama/embedding/whisper 均由 service daemon 管理，不需在此 kill
            {
                let ah = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    ah.exit(0);
                });
            }

            // 背景完成 vault 初始化、FileWatcher、server warmup
            let app_handle_sched = app_handle.clone();
            tauri::async_runtime::spawn(async move {
                let state = app_handle.state::<AppState>();

                // 載入已設定的 vault 路徑（via daemon API）
                let auth_tok = state.get_auth_token().await;
                let tok_ref: Option<&str> = if auth_tok.is_empty() { None } else { Some(&auth_tok) };
                if let Some(vp) = crate::api_client::daemon_get_setting(&state.http_client, tok_ref, "system_current_vault_path").await {
                    if !vp.is_empty() {
                        state.set_vault_path_with_agent(vp.clone()).await;

                        // 從 session 取得實際帳號名稱
                        let username = if let Ok(s) = crate::api_client::daemon_get::<serde_json::Value>(
                            &state.http_client, "/auth/session", tok_ref,
                        ).await {
                            s["username"].as_str().unwrap_or("").to_string()
                        } else { String::new() };
                        if !username.is_empty() {
                            state.set_username(username.clone()).await;
                        }

                        // 向 daemon 取得（或建立）vault UUID（傳 account_id 讓 register_vault 正確分區）
                        if let Ok(v) = crate::api_client::daemon_post::<_, serde_json::Value>(
                            &state.http_client,
                            "/vaults",
                            &serde_json::json!({"path": vp, "account_id": username}),
                            tok_ref,
                        ).await {
                            if let Some(uuid) = v["vault_id"].as_str() {
                                state.set_vault_uuid(uuid.to_string()).await;
                            }
                        }

                        // Agent 生命週期管理（daemon 負責，跳過）
                        // scheduled_tasks INSERT 已由 daemon 管理，跳過
                        let path = std::path::PathBuf::from(&vp);
                        if path.exists() {
                            // file watcher syncs external edits to daemon; startup scan skipped (daemon DB persists)
                            let stop_tx = vault::watcher::start_watcher(app_handle.clone(), path);
                            *state.watcher_stop.lock().await = Some(stop_tx);
                        }
                    }
                }

                // 初始化加密金鑰（daemon API 版本）
                // 必須在所有 encrypt/decrypt 呼叫之前執行
                let auth_tok2 = state.get_auth_token().await;
                let tok_ref2: Option<&str> = if auth_tok2.is_empty() { None } else { Some(&auth_tok2) };
                crate::crypto::init_encryption_key_daemon(&state.http_client, tok_ref2).await;

                // 標記 app 初始化完成，通知前端（不等 server warmup）
                state.app_ready.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = app_handle.emit("app:ready", serde_json::json!({}));

                // 訂閱 notetreelm-service SSE 事件，轉發為 Tauri emit
                {
                    let ah = app_handle.clone();
                    tauri::async_runtime::spawn(async move {
                        subscribe_service_events(ah).await;
                    });
                }

                // 以下為背景任務，不阻塞 app 載入

                // 預熱 whisper-server 與 llama-server（延遲 2 秒等前端 register 監聽器）
                tokio::time::sleep(Duration::from_secs(2)).await;
                tokio::join!(
                    warmup_whisper_server(&state, &app_handle),
                    warmup_llama_server(&state, &app_handle),
                    warmup_embedding_server(&state, &app_handle),
                );

                // AI server 狀態由 service 自行管理，Tauri 不再登記

                // seed-builtins 由 service auth 層在 login/register 時處理（per-account 幂等）
                // 通知前端刷新 agent/skill 面板
                let _ = app_handle.emit("agent:seeded", serde_json::json!({}));

                // 自動更新：掃描開啟 auto_update 的 import sessions
                auto_check_all_sessions(&app_handle, &state).await;
            });

            // 排程器：已由 daemon 負責管理，Tauri 端不再輪詢 scheduled_tasks
            // （移除 state.db 依賴，daemon service 層處理所有排程邏輯）
            let _ = app_handle_sched; // suppress unused variable warning

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
            restore_session,
            change_password,
            start_google_oauth,
            connect_google_calendar,
            get_calendar_status,
            disconnect_google_calendar,
            connect_gmail,
            get_gmail_status,
            disconnect_gmail,
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
            get_kb_chat_messages,
            save_kb_chat_messages,
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
            get_index_stats,
            reindex_vault_chunks,
            prepare_db_repair,
            list_repair_logs,
            search_vault_chunks,
            get_vault_uuid,
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
            query_kb,
            set_session_auto_update,
            debug_kb_chunks,
            suggest_kb_cards,
            get_kb_stats,
            get_aging_notes,
            mark_note_reviewed,
            list_kb_suggestions,
            dismiss_kb_suggestion,
            create_kb_card_note,
            get_brave_search_usage,
            sync_brave_key_id,
            save_knowledge_item,
            list_knowledge_items,
            get_knowledge_item,
            delete_knowledge_item,
            rename_knowledge_item,
            suggest_kb_cards_for_item,
            suggest_note_cards_for_item,
            suggest_skill_cards_for_item,
            get_cached_page,
            save_agent_skill,
            list_agent_skills,
            toggle_agent_skill,
            delete_agent_skill,
            update_agent_skill,
            add_skill_trigger,
            compress_conversation_to_knowledge,
            get_skill_usage_stats,
            extract_skill_from_exchange,
            // Voice
            transcribe_audio,
            stop_whisper_server,
            get_whisper_server_status,
            start_whisper_server,
            restart_whisper_server,
            // AI / LLM
            process_with_llm,
            confirm_write_tool,
            test_vault_tool,
            run_tool_pipeline,
            cancel_tool_test,
            stop_llama_server,
            get_llama_server_status,
            start_llama_server,
            restart_llama_server,
            get_embedding_server_status,
            start_embedding_server,
            stop_embedding_server,
            restart_embedding_server,
            check_embedding_endpoint,
            query_memory,
            rate_response,
            get_conversation_ratings,
            cancel_agent,
            invoke_agent,
            invoke_live_chat,
            set_note_status,
            // Agent Definitions
            list_agent_definitions,
            save_agent_definition,
            update_agent_definition,
            delete_agent_definition,
            toggle_agent_definition,
            wake_agent_definition,
            // Conversation management
            create_conversation,
            list_conversations,
            get_conversation,
            delete_conversation,
            update_conversation_title,
            get_or_create_live_chat_conversation,
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
            download_diarize_model,
            // Activity Pattern Learning
            save_pattern,
            update_pattern_score,
            list_patterns,
            decay_patterns,
            set_pattern_intent,
            // DevTools (debug helper)
            open_devtools,
            is_app_ready,
            // Daemon management
            commands::daemon::install_daemon_service,
            commands::daemon::uninstall_daemon_service,
            commands::daemon::get_daemon_status,
            commands::daemon::get_daemon_servers,
        ])
        .build(tauri::generate_context!())
        .expect("noteTreeLM 構建失敗")
        .run(|_app_handle, _event| {
            // llama/embedding/whisper 均由 service daemon 管理，不在 Tauri 端 kill
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
