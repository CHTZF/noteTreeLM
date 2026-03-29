/// runtime/tool_dispatch.rs
///
/// 工具執行實作：
///   resolve_vault_path / tool_list_structure / tool_read_note
///   inject_ai_frontmatter / set_frontmatter_key
///   tool_create_note / tool_update_note / tool_create_folder
///   is_write_tool / ensure_md / execute_vault_tool

use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};

/// 驗證相對路徑安全性（防止路徑穿越），返回絕對路徑
pub(crate) fn resolve_vault_path(rel_path: &str, vault_path: &str) -> Result<PathBuf, String> {
    if rel_path.contains("..") {
        return Err("不允許路徑穿越（..）".to_string());
    }
    let abs = PathBuf::from(vault_path).join(rel_path);
    if abs.starts_with(vault_path) {
        Ok(abs)
    } else {
        Err("路徑超出 Vault 範圍".to_string())
    }
}

/// 列出指定資料夾的子資料夾和 .md 筆記（單層）
pub(crate) fn tool_list_structure(rel_path: &str, vault_path: &str) -> String {
    let abs_path = if rel_path.is_empty() {
        PathBuf::from(vault_path)
    } else {
        match resolve_vault_path(rel_path, vault_path) {
            Ok(p) => p,
            Err(e) => return e,
        }
    };
    if !abs_path.is_dir() {
        return format!("路徑不存在或不是資料夾：{}", rel_path);
    }
    let mut folders: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&abs_path) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if entry.path().is_dir() {
                folders.push(format!("[📁] {}", name));
            } else if name.ends_with(".md") {
                notes.push(format!("[📄] {}", name));
            }
        }
    }
    folders.sort();
    notes.sort();
    let label = if rel_path.is_empty() { "根目錄".to_string() } else { rel_path.to_string() };
    let mut lines = vec![format!("📂 {} 的內容：", label)];
    lines.extend(folders);
    lines.extend(notes);
    if lines.len() == 1 {
        lines.push("（空）".to_string());
    }
    lines.join("\n")
}

/// 讀取筆記內容（最多 6000 字元）
pub(crate) fn tool_read_note(rel_path: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::read_to_string(&abs_path) {
        Ok(content) => {
            if content.len() > 6000 {
                // Snap to a valid char boundary — CJK chars are 3 bytes each,
                // so the raw byte index 6000 can land mid-character.
                let mut end = 6000usize;
                while end > 0 && !content.is_char_boundary(end) { end -= 1; }
                format!("{}\n\n[…內容過長，已截斷至約 6000 字元]", &content[..end])
            } else {
                content
            }
        }
        Err(e) => format!("讀取失敗：{}", e),
    }
}

// ── Frontmatter helpers ────────────────────────────────────────────────────

/// Inject `status: draft` + `created_by: ai` into frontmatter if no `status` field yet.
/// If content already has `status:`, leave it unchanged.
fn inject_ai_frontmatter(content: &str) -> String {
    let after = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        // No frontmatter — create one
        return format!("---\nstatus: draft\ncreated_by: ai\n---\n\n{}", content);
    };
    if let Some(end_offset) = content[after..].find("\n---") {
        let fm = &content[after..after + end_offset];
        if fm.lines().any(|l| l.trim_start().starts_with("status:")) {
            return content.to_string(); // Already has status — don't touch
        }
        let rest = &content[after + end_offset..]; // starts with "\n---..."
        format!("---\nstatus: draft\ncreated_by: ai\n{}{}", fm, rest)
    } else {
        format!("---\nstatus: draft\ncreated_by: ai\n---\n\n{}", content)
    }
}

/// Set (or insert) a single key in frontmatter. Creates frontmatter if absent.
pub(crate) fn set_frontmatter_key(content: &str, key: &str, value: &str) -> String {
    let prefix = format!("{}:", key);
    let new_line = format!("{}: {}", key, value);
    let after = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return format!("---\n{}: {}\n---\n\n{}", key, value, content);
    };
    if let Some(end_offset) = content[after..].find("\n---") {
        let fm = &content[after..after + end_offset];
        let rest = &content[after + end_offset..];
        let lines: Vec<&str> = fm.lines().collect();
        let idx = lines.iter().position(|l| l.trim_start().starts_with(&prefix));
        let new_fm = if let Some(i) = idx {
            let mut v = lines.clone();
            v[i] = &new_line;
            v.join("\n")
        } else {
            format!("{}\n{}", new_line, fm)
        };
        format!("---\n{}{}", new_fm, rest)
    } else {
        format!("---\n{}: {}\n---\n\n{}", key, value, content)
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// 建立新筆記（自動建立父資料夾）
pub(crate) async fn tool_create_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    _db_ctx: Option<()>,
) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Some(parent) = abs_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let final_content = inject_ai_frontmatter(content);
    if let Err(e) = tokio::fs::write(&abs_path, &final_content).await {
        return format!("建立失敗：{}", e);
    }
    format!("✅ 已建立筆記：{}", rel_path)
}

/// 更新現有筆記（覆寫全文）
pub(crate) async fn tool_update_note(
    rel_path: &str,
    content: &str,
    vault_path: &str,
    _db_ctx: Option<()>,
) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let final_content = inject_ai_frontmatter(content);
    if let Err(e) = tokio::fs::write(&abs_path, &final_content).await {
        return format!("更新失敗：{}", e);
    }
    format!("✅ 已更新筆記：{}", rel_path)
}

/// 建立資料夾
pub(crate) async fn tool_create_folder(rel_path: &str, vault_path: &str) -> String {
    let abs_path = match resolve_vault_path(rel_path, vault_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match tokio::fs::create_dir_all(&abs_path).await {
        Ok(_) => format!("✅ 已建立資料夾：{}", rel_path),
        Err(e) => format!("建立失敗：{}", e),
    }
}

/// 判斷工具是否為寫入操作（需要使用者確認）
pub(crate) fn is_write_tool(name: &str) -> bool {
    matches!(name, "create_note" | "update_note" | "create_folder")
}

/// 筆記路徑：若不以 .md 結尾則自動補上
pub(crate) fn ensure_md(path: &str) -> String {
    if path.is_empty() || path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{}.md", path)
    }
}


pub(crate) async fn execute_vault_tool(
    name: &str,
    args: &serde_json::Value,
    vault_path: &str,
    app: &AppHandle,
) -> String {
    if vault_path.is_empty() {
        return "Vault 未設定，無法執行 Vault 操作".to_string();
    }
    match name {
        "search_vault" => {
            let query = args["query"].as_str().unwrap_or("");
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() || query.is_empty() {
                return "搜尋失敗：未設定 Vault 或查詢為空".to_string();
            }
            let url = format!("/vaults/{}/search?q={}", urlencoding::encode(&vault_id), urlencoding::encode(query));
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(results) => {
                    let arr = results.as_array().cloned().unwrap_or_default();
                    if arr.is_empty() {
                        format!("搜尋「{}」：找不到相關筆記。", query)
                    } else {
                        let lines: Vec<String> = arr.iter().take(5).map(|r| {
                            format!("- **{}** ({})", r["title"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""))
                        }).collect();
                        format!("搜尋「{}」結果：\n{}", query, lines.join("\n"))
                    }
                }
                Err(_) => format!("搜尋「{}」失敗，請稍後再試。", query),
            }
        }
        "list_structure" => {
            let path = args["path"].as_str().unwrap_or("");
            tool_list_structure(path, vault_path)
        }
        "read_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            tool_read_note(&path, vault_path)
        }
        "create_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            let content = args["content"].as_str().unwrap_or("");
            let result = tool_create_note(&path, content, vault_path, None).await;
            // Sync to daemon after filesystem write
            {
                let st = app.state::<crate::state::AppState>();
                let vault_id = st.get_vault_uuid().await;
                let token = st.get_auth_token().await;
                let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
                if !vault_id.is_empty() && !path.is_empty() {
                    let abs = std::path::PathBuf::from(vault_path).join(&path);
                    if let Ok(c) = tokio::fs::read_to_string(&abs).await {
                        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                            &st.http_client,
                            &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                            &serde_json::json!({"path": path, "content": c}),
                            tok,
                        ).await;
                    }
                }
            }
            result
        }
        "update_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            let content = args["content"].as_str().unwrap_or("");
            let result = tool_update_note(&path, content, vault_path, None).await;
            // Sync to daemon after filesystem write
            {
                let st = app.state::<crate::state::AppState>();
                let vault_id = st.get_vault_uuid().await;
                let token = st.get_auth_token().await;
                let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
                if !vault_id.is_empty() && !path.is_empty() {
                    let abs = std::path::PathBuf::from(vault_path).join(&path);
                    if let Ok(c) = tokio::fs::read_to_string(&abs).await {
                        let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                            &st.http_client,
                            &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                            &serde_json::json!({"path": path, "content": c}),
                            tok,
                        ).await;
                    }
                }
            }
            result
        }
        "create_folder" => {
            let path = args["path"].as_str().unwrap_or("");
            let result = tool_create_folder(path, vault_path).await;
            // Trigger vault rescan so daemon indexes any new structure
            {
                let st = app.state::<crate::state::AppState>();
                let vault_id = st.get_vault_uuid().await;
                let token = st.get_auth_token().await;
                let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
                if !vault_id.is_empty() {
                    let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                        &st.http_client,
                        &format!("/vaults/{}/scan", urlencoding::encode(&vault_id)),
                        &serde_json::json!({}),
                        tok,
                    ).await;
                }
            }
            result
        }
        "query_memory" => {
            let keywords_val = &args["keywords"];
            let keywords: Vec<String> = if let Some(arr) = keywords_val.as_array() {
                arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect()
            } else if let Some(s) = keywords_val.as_str() {
                vec![s.to_string()]
            } else {
                vec![]
            };
            let limit = args["limit"].as_u64().unwrap_or(5).min(20);
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() {
                return "記憶查詢失敗：未設定 Vault".to_string();
            }
            // Use q= for semantic similarity search (server ranks by cosine via SurrealDB);
            // falls back to keyword string match if embedder is unavailable.
            let q_text = keywords.join(" ");
            let q_param = urlencoding::encode(&q_text).to_string();
            let url = format!(
                "/vaults/{}/memory/query?q={}&limit={}",
                urlencoding::encode(&vault_id), q_param, limit
            );
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(results) => {
                    let arr = results.as_array().cloned().unwrap_or_default();
                    if arr.is_empty() {
                        "記憶查詢：找不到相關記憶。".to_string()
                    } else {
                        let lines: Vec<String> = arr.iter().map(|r| {
                            let cat = r["category"].as_str().unwrap_or("general");
                            let content = r["content"].as_str().unwrap_or("");
                            format!("- [{}] {}", cat, content)
                        }).collect();
                        format!("記憶查詢結果：\n{}", lines.join("\n"))
                    }
                }
                Err(_) => "記憶查詢失敗，請稍後再試。".to_string(),
            }
        }
        "prefetch_memory" => {
            let context = args["context"].as_str().unwrap_or("").to_string();
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() {
                return "記憶預取失敗：未設定 Vault".to_string();
            }
            // Use q= for semantic similarity search; service falls back to keyword if no embedder.
            let q_param = urlencoding::encode(context.chars().take(120).collect::<String>().trim()).to_string();
            let url = format!(
                "/vaults/{}/memory/query?q={}&limit=8",
                urlencoding::encode(&vault_id), q_param
            );
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(results) => {
                    let arr = results.as_array().cloned().unwrap_or_default();
                    if arr.is_empty() {
                        String::new()
                    } else {
                        // Emit prefetched node_ids for MemoryLinksView highlight
                        let node_ids: Vec<String> = arr.iter()
                            .filter_map(|r| r["fact_id"].as_str())
                            .map(|fid| format!("memory:{}:{}", vault_id, fid))
                            .collect();
                        if !node_ids.is_empty() {
                            let _ = app.emit("memory:prefetched", serde_json::json!({
                                "node_ids": node_ids,
                                "source": "live_chat"
                            }));
                        }
                        let lines: Vec<String> = arr.iter().map(|r| {
                            let cat = r["category"].as_str().unwrap_or("general");
                            let content = r["content"].as_str().unwrap_or("");
                            format!("[{}] {}", cat, content)
                        }).collect();
                        format!("## 相關記憶\n{}", lines.join("\n"))
                    }
                }
                Err(_) => String::new(),
            }
        }
        "think" => {
            let thought = args["thought"].as_str().unwrap_or("").trim().to_string();
            if !thought.is_empty() {
                let _ = app.emit("live_chat:thinking", &thought);
            }
            String::new()
        }
        "find_related" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            if path.is_empty() {
                return "請提供筆記路徑".to_string();
            }
            let depth = args["depth"].as_u64().unwrap_or(1).min(2);
            let limit = args["limit"].as_u64().unwrap_or(10).min(30);
            let state = app.state::<crate::state::AppState>();
            let token = state.get_auth_token().await;
            let tok = if token.is_empty() { None } else { Some(token.as_str()) };
            let vault_id = state.get_vault_uuid().await;
            if vault_id.is_empty() {
                return "find_related 失敗：未設定 Vault".to_string();
            }
            let url = format!(
                "/vaults/{}/graph/related?path={}&depth={}&limit={}",
                urlencoding::encode(&vault_id),
                urlencoding::encode(&path),
                depth,
                limit
            );
            match crate::api_client::daemon_get::<serde_json::Value>(&state.http_client, &url, tok).await {
                Ok(data) => {
                    let nodes = data["nodes"].as_array().cloned().unwrap_or_default();
                    if nodes.is_empty() {
                        format!("「{}」在知識圖譜中沒有相關連結的筆記。", path)
                    } else {
                        let lines: Vec<String> = nodes.iter().map(|n| {
                            let label = n["label"].as_str().unwrap_or("(無標題)");
                            let fp = n["file_path"].as_str().unwrap_or("");
                            let rel = n["relation"].as_str().unwrap_or("link");
                            format!("- [{}] {} ({})", rel, label, fp)
                        }).collect();
                        format!("「{}」的相關筆記（深度 {}）：\n{}", path, depth, lines.join("\n"))
                    }
                }
                Err(e) => format!("find_related 失敗：{}", e),
            }
        }
        "open_note" => {
            let path = ensure_md(args["path"].as_str().unwrap_or(""));
            if path.is_empty() {
                return "請提供筆記路徑".to_string();
            }
            // 前端 openNote() 期望 relative path，直接傳入
            let _ = app.emit("ui:open_note", &path);
            format!("✅ 已打開筆記：{}", path)
        }
        _ => format!("未知工具：{}", name),
    }
}

/// 讀取最近對話，回傳摘要供 reflection agent 分析模式
pub async fn tool_list_recent_conversations(
    http_client: &reqwest::Client,
    auth_token: &str,
    limit: usize,
) -> String {
    let limit = limit.min(20);
    let tok = if auth_token.is_empty() { None } else { Some(auth_token) };
    let result: serde_json::Value = crate::api_client::daemon_get(
        http_client,
        &format!("/conversations?limit={}", limit),
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!([]));
    let rows = match result.as_array() {
        Some(r) => r.clone(),
        None => return "沒有找到任何對話記錄".to_string(),
    };
    if rows.is_empty() { return "沒有找到任何對話記錄".to_string(); }
    let mut out = format!("最近 {} 段對話：\n\n", rows.len());
    for (i, row) in rows.iter().enumerate() {
        let title = row["title"].as_str().unwrap_or("未命名");
        let mode  = row["mode"].as_str().unwrap_or("chat");
        out.push_str(&format!("## 對話 {} — {} ({})\n", i + 1, title, mode));
        if let Some(ref json) = row["messages_json"].as_str() {
            if let Ok(msgs) = serde_json::from_str::<Vec<serde_json::Value>>(json) {
                let tail: Vec<_> = msgs.iter()
                    .filter(|m| {
                        let role = m["role"].as_str().unwrap_or("");
                        role == "user" || role == "assistant"
                    })
                    .rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
                for m in tail {
                    let role    = m["role"].as_str().unwrap_or("?");
                    let content = m["content"].as_str().unwrap_or("").chars().take(200).collect::<String>();
                    out.push_str(&format!("**{}**: {}\n", role, content));
                }
            }
        }
        out.push('\n');
    }
    out
}
