use crate::{
    api_client::{daemon_delete, daemon_get, daemon_post},
    error::AppError,
    state::AppState,
    vault::{extract_title, count_words},
};

// ── Daemon sync helpers ───────────────────────────────────────────────────────

async fn daemon_index_note_vault(state: &AppState, vault_path: &str, rel_path: &str) {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return; }
    let abs = PathBuf::from(vault_path).join(rel_path);
    let content = match tokio::fs::read_to_string(&abs).await { Ok(c) => c, Err(_) => return };
    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
        &serde_json::json!({ "path": rel_path, "content": content }),
        tok,
    ).await;
}

async fn daemon_scan_vault(state: &AppState) {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return; }
    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
    let _ = daemon_post::<_, serde_json::Value>(
        &state.http_client,
        &format!("/vaults/{}/scan", urlencoding::encode(&vault_id)),
        &serde_json::json!({}),
        tok,
    ).await;
}

async fn daemon_delete_note_vault(state: &AppState, rel_path: &str) {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return; }
    let token = state.get_auth_token().await;
    let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
    let url = format!(
        "/vaults/{}/notes?path={}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(rel_path),
    );
    let _ = daemon_delete::<serde_json::Value>(&state.http_client, &url, tok).await;
}
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tauri::State;

/// 若 embedding server 正在運行，回傳其 base URL；否則回傳 None。
/// （embedding server 現由 service 管理；此 stub 保留給潛在呼叫端）
#[allow(dead_code)]
async fn embedding_url(_state: &AppState) -> Option<String> {
    None
}


#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Note {
    pub path: String,
    pub title: String,
    pub content: String,
    pub frontmatter: Option<String>,
    pub word_count: i64,
    pub created_at: i64,
    pub modified_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteResult {
    pub affected_links: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RenameResult {
    pub new_path: String,
    pub updated_files: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Link {
    pub id: String,
    pub source_path: String,
    pub target_title: String,
    pub target_path: Option<String>,
    pub link_type: String,
    pub raw_text: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    pub line_number: i64,
}

#[allow(dead_code)]
fn compute_checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}


#[tauri::command]
pub async fn create_note(
    state: State<'_, AppState>,
    title: String,
    folder: Option<String>,
    content: Option<String>,
) -> Result<Note, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    let content = content.unwrap_or_default();
    let folder = folder.unwrap_or_default();

    // 建立安全的檔案名稱
    let safe_title: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();

    let filename = format!("{}.md", safe_title.trim());
    let rel_path = if folder.is_empty() {
        filename.clone()
    } else {
        format!("{}/{}", folder.trim_end_matches('/'), filename)
    };

    let abs_path = PathBuf::from(&vault_path).join(&rel_path);

    // 檢查是否已有同名檔案（filesystem check，無需 DB）
    if abs_path.exists() {
        return Err(AppError::Vault(format!(
            "已存在同名筆記：「{}」，請使用其他名稱或不同資料夾。",
            title
        )));
    }

    if let Some(parent) = abs_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&abs_path, &content).await?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let word_count = count_words(&content);

    daemon_index_note_vault(&state, &vault_path, &rel_path).await;

    Ok(Note {
        path: rel_path,
        title,
        content,
        frontmatter: None,
        word_count,
        created_at: now_ms,
        modified_at: now_ms,
    })
}

#[tauri::command]
pub async fn read_note(
    state: State<'_, AppState>,
    path: String,
) -> Result<Note, AppError> {
    let vault_path = state.get_vault_path().await;
    let abs_path = PathBuf::from(&vault_path).join(&path);
    let content = tokio::fs::read_to_string(&abs_path).await
        .map_err(|_| AppError::Vault(format!("找不到筆記：{}", path)))?;
    let title = extract_title(&path, &content);
    let word_count = count_words(&content);
    let meta = abs_path.metadata().ok();
    let modified_ms = meta.as_ref()
        .and_then(|m| m.modified().ok())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64)
        .unwrap_or(0);
    Ok(Note {
        path,
        title,
        content,
        frontmatter: None,
        word_count,
        created_at: modified_ms,
        modified_at: modified_ms,
    })
}

#[tauri::command]
pub async fn update_note(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    let abs_path = PathBuf::from(&vault_path).join(&path);

    tokio::fs::write(&abs_path, &content).await?;

    daemon_index_note_vault(&state, &vault_path, &path).await;

    Ok(())
}


#[tauri::command]
pub async fn delete_note(
    state: State<'_, AppState>,
    path: String,
) -> Result<DeleteResult, AppError> {
    let vault_path = state.get_vault_path().await;

    // 刪除實體檔案
    let abs_path = PathBuf::from(&vault_path).join(&path);
    tokio::fs::remove_file(&abs_path).await.ok();

    daemon_delete_note_vault(&state, &path).await;

    Ok(DeleteResult { affected_links: 0 })
}

#[tauri::command]
pub async fn rename_note(
    state: State<'_, AppState>,
    path: String,
    new_title: String,
) -> Result<RenameResult, AppError> {
    let vault_path = state.get_vault_path().await;

    // 建立新路徑（保持原資料夾）
    let old_pathbuf = PathBuf::from(&path);
    let parent = old_pathbuf.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let safe_title: String = new_title
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let new_filename = format!("{}.md", safe_title.trim());
    let new_path = if parent.is_empty() {
        new_filename
    } else {
        format!("{}/{}", parent, new_filename)
    };

    // 取得舊標題（從檔案系統讀取）
    let abs_old = PathBuf::from(&vault_path).join(&path);
    let old_content = tokio::fs::read_to_string(&abs_old).await
        .map_err(|_| AppError::Vault(format!("找不到筆記：{}", path)))?;
    let old_title = extract_title(&path, &old_content);

    // 更新每個引用舊標題的筆記（filesystem scan — backlinks from daemon not available yet）
    let updated_files: Vec<String> = Vec::new();
    // Note: backlink update skipped — daemon handles link re-indexing after rename

    // 重新命名實體檔案
    let abs_new = PathBuf::from(&vault_path).join(&new_path);
    if let Some(parent) = abs_new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&abs_old, &abs_new).await?;

    let _ = old_title; // suppress unused warning
    daemon_delete_note_vault(&state, &path).await;
    daemon_index_note_vault(&state, &vault_path, &new_path).await;

    Ok(RenameResult { new_path, updated_files })
}

#[tauri::command]
pub async fn list_notes(
    state: State<'_, AppState>,
    folder: Option<String>,
) -> Result<Vec<Note>, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Ok(vec![]);
    }

    let prefix_filter = folder.map(|f| format!("{}/", f.trim_end_matches('/')));

    let mut notes = Vec::new();
    collect_notes_fs(&vault_path, &vault_path, &prefix_filter, &mut notes).await?;

    // Sort by modified_at descending
    notes.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    Ok(notes)
}

async fn collect_notes_fs(
    vault_root: &str,
    dir: &str,
    prefix_filter: &Option<String>,
    notes: &mut Vec<Note>,
) -> Result<(), AppError> {
    let mut entries = tokio::fs::read_dir(dir).await
        .map_err(|e| AppError::Vault(e.to_string()))?;
    while let Some(entry) = entries.next_entry().await
        .map_err(|e| AppError::Vault(e.to_string()))? {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') || name == "assets" {
            continue;
        }
        if path.is_dir() {
            Box::pin(collect_notes_fs(vault_root, &path.to_string_lossy(), prefix_filter, notes)).await?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            let rel_path = match crate::vault::to_relative_path(vault_root, &path) {
                Some(p) => p,
                None => continue,
            };
            if let Some(ref prefix) = prefix_filter {
                if !rel_path.starts_with(prefix.as_str()) {
                    continue;
                }
            }
            let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            let title = extract_title(&rel_path, &content);
            let word_count = count_words(&content);
            let meta = path.metadata().ok();
            let modified_ms = meta.as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64)
                .unwrap_or(0);
            notes.push(Note {
                path: rel_path,
                title,
                content,
                frontmatter: None,
                word_count,
                created_at: modified_ms,
                modified_at: modified_ms,
            });
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn get_backlinks(
    state: State<'_, AppState>,
    title: String,
) -> Result<Vec<Link>, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() { return Ok(vec![]); }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!(
        "/vaults/{}/backlinks?title={}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(&title),
    );
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!([]));
    let arr = result.as_array().cloned().unwrap_or_default();
    let links = arr
        .iter()
        .filter_map(|v| {
            Some(Link {
                id: v["link_id"].as_str()
                    .or_else(|| v["id"].as_str())
                    .unwrap_or("").to_string(),
                source_path: v["source_path"].as_str()?.to_string(),
                target_title: v["target_title"].as_str().unwrap_or("").to_string(),
                target_path: v["target_path"].as_str().map(|s| s.to_string()),
                link_type: v["link_type"].as_str().unwrap_or("wiki").to_string(),
                raw_text: v["raw_text"].as_str().unwrap_or("").to_string(),
                alias: v["alias"].as_str().map(|s| s.to_string()),
                heading: v["heading"].as_str().map(|s| s.to_string()),
                line_number: v["line_number"].as_i64().unwrap_or(0),
            })
        })
        .collect();
    Ok(links)
}

/// 掃描整個 Vault — DB indexing is handled by the daemon file watcher
#[tauri::command]
pub async fn scan_vault(state: State<'_, AppState>) -> Result<usize, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    // Count .md files on disk
    let mut count = 0usize;
    count_md_files(&vault_path, &vault_path, &mut count).await?;

    // Trigger daemon to re-scan (updates daemon DB + embeddings)
    let vault_id = state.get_vault_uuid().await;
    if !vault_id.is_empty() {
        let token = state.get_auth_token().await;
        let tok = if token.is_empty() { None } else { Some(token.as_str()) };
        let _ = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/scan", urlencoding::encode(&vault_id)),
            &serde_json::json!({}),
            tok,
        ).await;
    }

    Ok(count)
}

async fn count_md_files(vault_root: &str, dir: &str, count: &mut usize) -> Result<(), AppError> {
    let mut entries = tokio::fs::read_dir(dir).await
        .map_err(|e| AppError::Vault(e.to_string()))?;
    while let Some(entry) = entries.next_entry().await
        .map_err(|e| AppError::Vault(e.to_string()))? {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') || name == "assets" {
            continue;
        }
        if path.is_dir() {
            Box::pin(count_md_files(vault_root, &path.to_string_lossy(), count)).await?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            *count += 1;
        }
    }
    Ok(())
}

/// 移動筆記到不同資料夾（保持檔案名稱與標題不變）
#[tauri::command]
pub async fn move_note(
    state: State<'_, AppState>,
    old_path: String,
    new_folder: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;

    let filename = PathBuf::from(&old_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| AppError::Vault("無效的來源路徑".to_string()))?;

    let new_path = if new_folder.is_empty() {
        filename.clone()
    } else {
        format!("{}/{}", new_folder.trim_end_matches('/'), filename)
    };

    if new_path == old_path {
        return Ok(old_path);
    }

    let abs_old = PathBuf::from(&vault_path).join(&old_path);
    let abs_new = PathBuf::from(&vault_path).join(&new_path);

    if let Some(parent) = abs_new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&abs_old, &abs_new).await
        .map_err(|e| AppError::Vault(format!("移動失敗：{}", e)))?;

    daemon_delete_note_vault(&state, &old_path).await;
    daemon_index_note_vault(&state, &vault_path, &new_path).await;

    Ok(new_path)
}

/// 讀取任意本地圖片為 base64 字串（供預覽區使用）
/// 將整個資料夾（含子資料夾與筆記）移動到新的父資料夾
#[tauri::command]
pub async fn move_folder(
    state: State<'_, AppState>,
    folder_path: String,   // 舊相對路徑，e.g. "projects"
    new_parent: String,    // 新父資料夾相對路徑（空 = 根目錄），e.g. "archive"
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;

    if folder_path.is_empty() || folder_path.contains("..") {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }
    // 防止移動到自身或子資料夾
    if !new_parent.is_empty()
        && (new_parent == folder_path || new_parent.starts_with(&format!("{}/", folder_path)))
    {
        return Err(AppError::Vault(
            "不能將資料夾移動到自身或其子資料夾".to_string(),
        ));
    }

    let folder_name = PathBuf::from(&folder_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| AppError::Vault("無效的資料夾名稱".to_string()))?;

    let new_folder_path = if new_parent.is_empty() {
        folder_name.clone()
    } else {
        format!("{}/{}", new_parent.trim_end_matches('/'), folder_name)
    };

    if new_folder_path == folder_path {
        return Ok(folder_path);
    }

    let abs_old = PathBuf::from(&vault_path).join(&folder_path);
    let abs_new = PathBuf::from(&vault_path).join(&new_folder_path);

    if let Some(parent) = abs_new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&abs_old, &abs_new)
        .await
        .map_err(|e| AppError::Vault(format!("移動資料夾失敗：{}", e)))?;

    daemon_scan_vault(&state).await;

    Ok(new_folder_path)
}

/// 重新命名資料夾（保留在原父資料夾，只改目錄名稱）
#[tauri::command]
pub async fn rename_folder(
    state: State<'_, AppState>,
    folder_path: String, // 舊相對路徑，e.g. "projects/old-name"
    new_name: String,    // 新目錄名稱，e.g. "new-name"
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;

    let new_name = new_name.trim().to_string();
    if folder_path.is_empty() || folder_path.contains("..") || new_name.is_empty() || new_name.contains('/') || new_name.contains("..") {
        return Err(AppError::Vault("無效的資料夾路徑或名稱".to_string()));
    }

    let parent = PathBuf::from(&folder_path)
        .parent()
        .and_then(|p| {
            let s = p.to_string_lossy().to_string();
            if s.is_empty() { None } else { Some(s) }
        });

    let new_folder_path = if let Some(p) = parent {
        format!("{}/{}", p, new_name)
    } else {
        new_name.clone()
    };

    if new_folder_path == folder_path {
        return Ok(folder_path);
    }

    let abs_old = PathBuf::from(&vault_path).join(&folder_path);
    let abs_new = PathBuf::from(&vault_path).join(&new_folder_path);

    tokio::fs::rename(&abs_old, &abs_new)
        .await
        .map_err(|e| AppError::Vault(format!("重新命名資料夾失敗：{}", e)))?;

    daemon_scan_vault(&state).await;

    Ok(new_folder_path)
}

/// 相對路徑由前端以 vault_path 補完後傳入
#[tauri::command]
pub async fn read_file_base64(path: String) -> Result<String, AppError> {
    let bytes = std::fs::read(&path)
        .map_err(|e| AppError::Vault(format!("無法讀取圖片 {}: {}", path, e)))?;
    Ok(BASE64.encode(&bytes))
}

/// 以相對路徑讀取 Vault 中的檔案（base64）
/// 使用 State 取得 vault_path，再以 PathBuf::join 組合，
/// 完全由 Rust 處理路徑分隔符，不依賴前端字串拼接。
#[tauri::command]
pub async fn read_vault_file_base64(
    state: State<'_, AppState>,
    rel_path: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    let abs_path = PathBuf::from(&vault_path).join(&rel_path);
    let bytes = std::fs::read(&abs_path)
        .map_err(|e| AppError::Vault(format!("無法讀取檔案 {}: {}", abs_path.display(), e)))?;
    Ok(BASE64.encode(&bytes))
}

/// 在 Vault 中建立資料夾（包括空資料夾）
#[tauri::command]
pub async fn create_folder(
    state: State<'_, AppState>,
    folder_path: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }
    let abs_path = PathBuf::from(&vault_path).join(&folder_path);
    tokio::fs::create_dir_all(&abs_path).await
        .map_err(|e| AppError::Vault(format!("建立資料夾失敗：{}", e)))?;
    Ok(())
}

/// 列出 Vault 中所有資料夾路徑（含空資料夾）
#[tauri::command]
pub async fn list_folders(
    state: State<'_, AppState>,
) -> Result<Vec<String>, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Ok(vec![]);
    }
    let mut folders = Vec::new();
    collect_folders(&vault_path, &vault_path, &mut folders).await?;
    Ok(folders)
}

async fn collect_folders(
    vault_root: &str,
    dir: &str,
    folders: &mut Vec<String>,
) -> Result<(), AppError> {
    let mut entries = tokio::fs::read_dir(dir).await
        .map_err(|e| AppError::Vault(format!("無法讀取目錄：{}", e)))?;
    while let Some(entry) = entries.next_entry().await
        .map_err(|e| AppError::Vault(e.to_string()))? {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name.starts_with('.') || name == "assets" {
                continue;
            }
            if let Some(rel) = crate::vault::to_relative_path(vault_root, &path) {
                folders.push(rel);
                Box::pin(collect_folders(vault_root, &path.to_string_lossy(), folders)).await?;
            }
        }
    }
    Ok(())
}

/// 刪除資料夾及其下所有筆記，回傳刪除的筆記數量
#[tauri::command]
pub async fn delete_folder(
    state: State<'_, AppState>,
    folder_path: String,
) -> Result<u32, AppError> {
    let vault_path = state.get_vault_path().await;

    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }

    // Count files before deletion for return value
    let mut count = 0usize;
    let abs_path = PathBuf::from(&vault_path).join(&folder_path);
    count_md_files(&vault_path, &abs_path.to_string_lossy(), &mut count).await.ok();

    // 刪除實體目錄（遞迴）
    tokio::fs::remove_dir_all(&abs_path).await
        .map_err(|e| AppError::Vault(format!("刪除資料夾失敗：{}", e)))?;

    daemon_scan_vault(&state).await;

    Ok(count as u32)
}

/// 將任意檔案複製到 Vault（指定資料夾，預設根目錄）
#[tauri::command]
pub async fn import_image(
    state: State<'_, AppState>,
    source_path: String,
    folder: Option<String>,
    new_name: Option<String>,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    let orig_filename = PathBuf::from(&source_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| AppError::Vault("無效的檔案路徑".to_string()))?;
    // If new_name provided, use it; preserve original extension if new_name has no extension
    let filename = if let Some(name) = new_name.filter(|n| !n.trim().is_empty()) {
        let name = name.trim().to_string();
        if PathBuf::from(&name).extension().is_some() {
            name
        } else {
            let orig_ext = PathBuf::from(&orig_filename)
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            if orig_ext.is_empty() { name } else { format!("{}.{}", name, orig_ext) }
        }
    } else {
        orig_filename
    };
    let folder = folder.unwrap_or_default();
    let rel_path = if folder.is_empty() {
        filename.clone()
    } else {
        format!("{}/{}", folder.trim_end_matches('/'), filename)
    };
    let dest = PathBuf::from(&vault_path).join(&rel_path);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::copy(&source_path, &dest).await
        .map_err(|e| AppError::Vault(format!("匯入圖片失敗：{}", e)))?;
    Ok(rel_path)
}

/// 將 base64 bytes 寫入 Vault 指定資料夾，回傳相對路徑（用於前端 paste 上傳）
#[tauri::command]
pub async fn import_file_from_bytes(
    state: State<'_, AppState>,
    filename: String,
    folder: String,
    data_base64: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err(AppError::Vault("無效的檔名".to_string()));
    }
    use base64::{Engine as _, engine::general_purpose};
    let bytes = general_purpose::STANDARD
        .decode(data_base64.trim())
        .map_err(|e| AppError::Vault(format!("base64 解碼失敗：{}", e)))?;

    let rel_path = if folder.is_empty() {
        filename.clone()
    } else {
        format!("{}/{}", folder.trim_end_matches('/'), filename)
    };
    let dest = PathBuf::from(&vault_path).join(&rel_path);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&dest, &bytes).await
        .map_err(|e| AppError::Vault(format!("寫入檔案失敗：{}", e)))?;

    daemon_scan_vault(&state).await;
    Ok(rel_path)
}

/// 列出 Vault 中所有圖片資源的相對路徑
#[tauri::command]
pub async fn list_assets(
    state: State<'_, AppState>,
) -> Result<Vec<String>, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Ok(vec![]);
    }
    let mut assets = Vec::new();
    collect_assets(&vault_path, &vault_path, &mut assets).await?;
    Ok(assets)
}

async fn collect_assets(
    vault_root: &str,
    dir: &str,
    assets: &mut Vec<String>,
) -> Result<(), AppError> {
    let mut entries = tokio::fs::read_dir(dir).await
        .map_err(|e| AppError::Vault(e.to_string()))?;
    while let Some(entry) = entries.next_entry().await
        .map_err(|e| AppError::Vault(e.to_string()))? {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') { continue; }
        if path.is_dir() {
            Box::pin(collect_assets(vault_root, &path.to_string_lossy(), assets)).await?;
        } else {
            let ext = path.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            // 跳過 Markdown 筆記（已由 notes 系統管理）
            if matches!(ext.as_str(), "md" | "markdown" | "mdx") {
                continue;
            }
            if let Some(rel) = crate::vault::to_relative_path(vault_root, &path) {
                assets.push(rel);
            }
        }
    }
    Ok(())
}

/// 從 URL 下載圖片到 vault/assets/ 資料夾，回傳相對路徑
#[tauri::command]
pub async fn download_asset_to_vault(
    state: State<'_, AppState>,
    url: String,
    new_name: Option<String>,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("noteTreeLM/1.0")
        .build()
        .map_err(|e| AppError::Import(e.to_string()))?;

    let resp = client.get(&url).send().await
        .map_err(|e| AppError::Import(format!("下載失敗：{}", e)))?;

    // 驗證 Content-Type 為圖片
    let content_type = resp.headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !content_type.starts_with("image/") {
        return Err(AppError::Vault(format!("URL 回應不是圖片（Content-Type: {}）", content_type)));
    }

    // 從 URL 取得副檔名
    let raw_name = url.split('?').next().unwrap_or(&url)
        .split('/').last().unwrap_or("image");
    let url_ext = if raw_name.contains('.') {
        raw_name.split('.').last().unwrap_or("").to_string()
    } else {
        String::new()
    };
    let ct_ext = {
        let e = content_type.split('/').nth(1).unwrap_or("png");
        e.split(';').next().unwrap_or("png").to_string()
    };

    // 決定最終檔名：優先使用 new_name（保留副檔名邏輯）
    let filename = if let Some(name) = new_name.filter(|n| !n.trim().is_empty()) {
        let name = name.trim().to_string();
        if PathBuf::from(&name).extension().is_some() {
            name
        } else {
            let ext = if !url_ext.is_empty() { url_ext } else { ct_ext };
            format!("{}.{}", name, ext)
        }
    } else if raw_name.contains('.') {
        raw_name.to_string()
    } else {
        format!("{}.{}", raw_name, ct_ext)
    };

    let bytes = resp.bytes().await
        .map_err(|e| AppError::Import(format!("讀取內容失敗：{}", e)))?;

    let assets_dir = PathBuf::from(&vault_path).join("assets");
    tokio::fs::create_dir_all(&assets_dir).await
        .map_err(|e| AppError::Io(e.to_string()))?;

    // 避免覆蓋同名檔案：加上數字後綴
    let dest_path = PathBuf::from(&assets_dir).join(&filename);
    let final_path = if dest_path.exists() {
        let stem = PathBuf::from(&filename)
            .file_stem().unwrap_or_default().to_string_lossy().to_string();
        let ext = PathBuf::from(&filename)
            .extension().unwrap_or_default().to_string_lossy().to_string();
        let mut i = 1u32;
        loop {
            let candidate = assets_dir.join(format!("{}_{}.{}", stem, i, ext));
            if !candidate.exists() { break candidate; }
            i += 1;
        }
    } else {
        dest_path
    };

    let rel_filename = final_path.file_name()
        .unwrap_or_default().to_string_lossy().to_string();
    tokio::fs::write(&final_path, &bytes).await
        .map_err(|e| AppError::Io(e.to_string()))?;

    Ok(format!("assets/{}", rel_filename))
}

/// 刪除 Vault 中的圖片資源
#[tauri::command]
pub async fn delete_asset(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    if path.contains("..") {
        return Err(AppError::Vault("無效的路徑".to_string()));
    }
    let abs_path = PathBuf::from(&vault_path).join(&path);
    tokio::fs::remove_file(&abs_path).await
        .map_err(|e| AppError::Vault(format!("刪除圖片失敗：{}", e)))?;
    Ok(())
}

/// 重命名 Vault 中的檔案資源（圖片等）
#[tauri::command]
pub async fn rename_asset(
    state: State<'_, AppState>,
    path: String,
    new_name: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if path.contains("..") || new_name.contains("..") || new_name.contains('/') || new_name.contains('\\') {
        return Err(AppError::Vault("無效的路徑或名稱".to_string()));
    }
    let abs_path = PathBuf::from(&vault_path).join(&path);
    let parent = abs_path.parent()
        .ok_or_else(|| AppError::Vault("無法取得父目錄".to_string()))?;
    let new_abs_path = parent.join(&new_name);
    if new_abs_path.exists() {
        return Err(AppError::Vault(format!("檔案 {} 已存在", new_name)));
    }
    tokio::fs::rename(&abs_path, &new_abs_path).await
        .map_err(|e| AppError::Vault(format!("重命名失敗：{}", e)))?;
    // 回傳新的相對路徑
    let new_rel = crate::vault::to_relative_path(&vault_path, &new_abs_path)
        .ok_or_else(|| AppError::Vault("無法計算新路徑".to_string()))?;
    Ok(new_rel)
}

/// 使用系統預設程式開啟指定的 Vault 內部檔案
#[tauri::command]
pub async fn open_path_externally(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    if path.contains("..") {
        return Err(AppError::Vault("無效的路徑".to_string()));
    }
    let abs_path = PathBuf::from(&vault_path).join(&path);
    if !abs_path.exists() {
        return Err(AppError::Vault(format!("檔案不存在：{}", path)));
    }
    #[cfg(target_os = "macos")]
    {
        tokio::process::Command::new("open")
            .arg(&abs_path)
            .spawn()
            .map_err(|e| AppError::Vault(format!("無法開啟檔案：{}", e)))?;
    }
    #[cfg(target_os = "windows")]
    {
        tokio::process::Command::new("explorer")
            .arg(&abs_path)
            .spawn()
            .map_err(|e| AppError::Vault(format!("無法開啟檔案：{}", e)))?;
    }
    #[cfg(target_os = "linux")]
    {
        tokio::process::Command::new("xdg-open")
            .arg(&abs_path)
            .spawn()
            .map_err(|e| AppError::Vault(format!("無法開啟檔案：{}", e)))?;
    }
    Ok(())
}

// ─────────────────────────────── Trash ──────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TrashItem {
    pub id: String,
    pub original_path: String,
    pub name: String,
    pub title: String,
    pub trash_filename: String,
    pub deleted_at: i64,
}

/// 將單一筆記移入 .trash/ 目錄（內部輔助函式）
/// Writes a JSON sidecar (.trash/<trash_filename>.meta.json) to track metadata
async fn trash_single_note(
    vault_path: &str,
    note_path: &str,
) -> Result<(), AppError> {
    let abs_path = PathBuf::from(vault_path).join(note_path);
    let content = tokio::fs::read_to_string(&abs_path).await.unwrap_or_default();
    let title = extract_title(note_path, &content);

    let filename = PathBuf::from(note_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "note.md".to_string());

    let trash_dir = PathBuf::from(vault_path).join(".trash");
    tokio::fs::create_dir_all(&trash_dir)
        .await
        .map_err(|e| AppError::Vault(format!("無法建立垃圾桶目錄：{}", e)))?;

    // 避免檔名衝突：加時間戳後綴
    let trash_filename = if trash_dir.join(&filename).exists() {
        let stem = PathBuf::from(&filename)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ts = chrono::Utc::now().timestamp_millis();
        format!("{}_{}.md", stem, ts)
    } else {
        filename.clone()
    };

    if abs_path.exists() {
        tokio::fs::rename(&abs_path, trash_dir.join(&trash_filename))
            .await
            .map_err(|e| AppError::Vault(format!("移動到垃圾桶失敗：{}", e)))?;
    }

    // Write JSON sidecar to persist trash metadata (no DB required)
    let item_id = uuid::Uuid::new_v4().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let meta = serde_json::json!({
        "item_id": item_id,
        "original_path": note_path,
        "name": filename,
        "title": title,
        "trash_filename": trash_filename,
        "deleted_at": now_ms,
    });
    let meta_path = trash_dir.join(format!("{}.meta.json", trash_filename));
    let _ = tokio::fs::write(&meta_path, meta.to_string()).await;

    Ok(())
}

/// Load trash metadata from JSON sidecars in .trash/
async fn load_trash_items(vault_path: &str) -> Vec<TrashItem> {
    let trash_dir = PathBuf::from(vault_path).join(".trash");
    let mut items = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(&trash_dir).await else { return items; };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if !name.ends_with(".meta.json") { continue; }
        if let Ok(raw) = tokio::fs::read_to_string(&path).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                items.push(TrashItem {
                    id: v["item_id"].as_str().unwrap_or("").to_string(),
                    original_path: v["original_path"].as_str().unwrap_or("").to_string(),
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    title: v["title"].as_str().unwrap_or("").to_string(),
                    trash_filename: v["trash_filename"].as_str().unwrap_or("").to_string(),
                    deleted_at: v["deleted_at"].as_i64().unwrap_or(0),
                });
            }
        }
    }
    items.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    items
}

/// 將單一筆記移至垃圾桶（軟刪除）
#[tauri::command]
pub async fn trash_note(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    if path.contains("..") {
        return Err(AppError::Vault("無效的路徑".to_string()));
    }
    trash_single_note(&vault_path, &path).await?;
    daemon_delete_note_vault(&state, &path).await;
    Ok(())
}

/// 將資料夾中所有筆記移至垃圾桶，然後刪除實體資料夾
#[tauri::command]
pub async fn trash_folder(
    state: State<'_, AppState>,
    folder_path: String,
) -> Result<u32, AppError> {
    let vault_path = state.get_vault_path().await;

    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }

    // Collect .md files under folder for trashing
    let prefix = format!("{}/", folder_path.trim_end_matches('/'));
    let mut md_paths: Vec<String> = Vec::new();
    collect_md_paths(&vault_path, &vault_path, &prefix, &mut md_paths).await?;

    let count = md_paths.len() as u32;
    for note_path in &md_paths {
        trash_single_note(&vault_path, note_path).await?;
        daemon_delete_note_vault(&state, note_path).await;
    }

    let abs_path = PathBuf::from(&vault_path).join(&folder_path);
    if abs_path.exists() {
        tokio::fs::remove_dir_all(&abs_path)
            .await
            .map_err(|e| AppError::Vault(format!("刪除資料夾失敗：{}", e)))?;
    }

    Ok(count)
}

async fn collect_md_paths(
    vault_root: &str,
    dir: &str,
    prefix: &str,
    paths: &mut Vec<String>,
) -> Result<(), AppError> {
    let mut entries = tokio::fs::read_dir(dir).await
        .map_err(|e| AppError::Vault(e.to_string()))?;
    while let Some(entry) = entries.next_entry().await
        .map_err(|e| AppError::Vault(e.to_string()))? {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') { continue; }
        if path.is_dir() {
            Box::pin(collect_md_paths(vault_root, &path.to_string_lossy(), prefix, paths)).await?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            if let Some(rel) = crate::vault::to_relative_path(vault_root, &path) {
                if rel.starts_with(prefix) {
                    paths.push(rel);
                }
            }
        }
    }
    Ok(())
}

/// 列出垃圾桶中所有項目（依刪除時間降序）
#[tauri::command]
pub async fn list_trash(
    state: State<'_, AppState>,
) -> Result<Vec<TrashItem>, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Ok(vec![]);
    }
    let mut items = load_trash_items(&vault_path).await;
    items.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at));
    Ok(items)
}

/// 復原垃圾桶項目到指定資料夾，回傳新路徑
#[tauri::command]
pub async fn restore_trash_item(
    state: State<'_, AppState>,
    id: String,
    target_folder: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }

    // Look up the trash file by scanning the .trash directory for a matching id prefix
    let trash_dir = PathBuf::from(&vault_path).join(".trash");
    let mut trash_filename = String::new();
    let mut item_name = String::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&trash_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.starts_with(&id) || fname.contains(&id) {
                trash_filename = fname.clone();
                // derive name from filename (strip id prefix if present)
                item_name = fname.trim_start_matches(&format!("{}_", id)).to_string();
                break;
            }
        }
    }
    if trash_filename.is_empty() {
        // Fallback: use id as filename directly
        let candidate_file = trash_dir.join(&id);
        if candidate_file.exists() {
            trash_filename = id.clone();
            item_name = id.clone();
        } else {
            return Err(AppError::Vault("找不到垃圾桶項目".to_string()));
        }
    }

    let item = TrashItem {
        id: id.clone(),
        original_path: item_name.clone(),
        name: item_name.clone(),
        title: item_name.clone(),
        trash_filename: trash_filename.clone(),
        deleted_at: 0,
    };

    let candidate = if target_folder.is_empty() {
        item.name.clone()
    } else {
        format!("{}/{}", target_folder.trim_end_matches('/'), item.name)
    };

    // 若目標已存在則加時間戳後綴
    let new_path = if PathBuf::from(&vault_path).join(&candidate).exists() {
        let stem = PathBuf::from(&item.name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ts = chrono::Utc::now().timestamp_millis();
        let suffixed = format!("{}_{}.md", stem, ts);
        if target_folder.is_empty() { suffixed } else {
            format!("{}/{}", target_folder.trim_end_matches('/'), suffixed)
        }
    } else {
        candidate
    };

    let trash_file = PathBuf::from(&vault_path)
        .join(".trash")
        .join(&item.trash_filename);

    let abs_new = PathBuf::from(&vault_path).join(&new_path);
    if let Some(parent) = abs_new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&trash_file, &abs_new)
        .await
        .map_err(|e| AppError::Vault(format!("復原失敗：{}", e)))?;

    daemon_index_note_vault(&state, &vault_path, &new_path).await;
    Ok(new_path)
}

/// 徹底刪除垃圾桶中的項目（不可復原）
#[tauri::command]
pub async fn delete_trash_items(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    let trash_dir = PathBuf::from(&vault_path).join(".trash");

    for id in &ids {
        // Scan .trash dir for files matching the id
        if let Ok(mut entries) = tokio::fs::read_dir(&trash_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.starts_with(id.as_str()) || fname.contains(id.as_str()) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                    break;
                }
            }
        }
    }

    Ok(())
}

/// 取得 chunk 索引統計（用於前端顯示進度）
#[tauri::command]
pub async fn get_index_stats(state: State<'_, AppState>) -> Result<serde_json::Value, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        return Ok(serde_json::json!({ "total": 0, "chunked": 0, "embedded": 0 }));
    }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/stats", urlencoding::encode(&vault_id));
    let result = daemon_get::<serde_json::Value>(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!({ "total": 0, "chunked": 0, "embedded": 0 }));
    Ok(result)
}

/// 列出所有修復 log（app_data_dir/db_repair_logs/*.json），由新到舊
#[tauri::command]
pub async fn list_repair_logs(app: tauri::AppHandle) -> Result<Vec<serde_json::Value>, AppError> {
    use tauri::Manager;
    let logs_dir = app.path().app_data_dir()
        .map_err(|e: tauri::Error| AppError::Database(e.to_string()))?
        .join("db_repair_logs");

    if !logs_dir.exists() {
        return Ok(vec![]);
    }

    let mut entries = tokio::fs::read_dir(&logs_dir).await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let mut files: Vec<(String, String)> = vec![]; // (filename, content)
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".json") { continue }
        if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
            files.push((name, content));
        }
    }
    // 按檔名（時間戳）降序
    files.sort_by(|a, b| b.0.cmp(&a.0));

    let mut logs = vec![];
    for (filename, content) in files {
        if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&content) {
            v["filename"] = serde_json::Value::String(filename);
            logs.push(v);
        }
    }
    Ok(logs)
}

/// 備份重要 tables → 寫入 db_needs_repair flag，使用者重啟後自動重建 DB
#[tauri::command]
pub async fn prepare_db_repair(
    _state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), AppError> {
    use tauri::Manager;
    let app_data_dir = app.path().app_data_dir()
        .map_err(|e: tauri::Error| AppError::Database(e.to_string()))?;

    // DB backup is handled by daemon; just write the repair flag.
    tokio::fs::write(app_data_dir.join("db_needs_repair"), "1")
        .await
        .map_err(|e: std::io::Error| AppError::Database(e.to_string()))?;

    Ok(())
}

/// 重新建立整個 vault 的 chunk 索引（委派 daemon rescan）
#[tauri::command]
pub async fn reindex_vault_chunks(
    state: State<'_, AppState>,
    _app: tauri::AppHandle,
) -> Result<usize, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Ok(0);
    }
    // Count .md files
    let mut count = 0usize;
    count_md_files(&vault_path, &vault_path, &mut count).await?;

    // Trigger daemon rescan
    let vault_id = state.get_vault_uuid().await;
    if !vault_id.is_empty() {
        let token = state.get_auth_token().await;
        let tok = if token.is_empty() { None } else { Some(token.as_str()) };
        let _ = daemon_post::<_, serde_json::Value>(
            &state.http_client,
            &format!("/vaults/{}/scan", urlencoding::encode(&vault_id)),
            &serde_json::json!({}),
            tok,
        ).await;
    }
    Ok(count)
}

/// 語意搜尋：委派 daemon search API（供前端 SemanticSearchPanel 使用）
#[tauri::command]
pub async fn search_vault_chunks(
    state: State<'_, AppState>,
    query: String,
    #[allow(non_snake_case)]
    verifiedOnly: Option<bool>,
) -> Result<String, AppError> {
    if query.trim().is_empty() {
        return Ok(String::new());
    }
    let vault_id = state.get_vault_id().await?;
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };

    let mut search_url = format!(
        "/vaults/{}/search?q={}",
        urlencoding::encode(&vault_id),
        urlencoding::encode(query.trim()),
    );
    if verifiedOnly.unwrap_or(false) {
        search_url.push_str("&verified_only=true");
    }

    let result: serde_json::Value = daemon_get(
        &state.http_client,
        &search_url,
        tok,
    ).await.unwrap_or_else(|_| serde_json::json!([]));

    let rows = result.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        return Ok("🔍 daemon\nno_results".to_string());
    }

    let mut lines = vec![
        "🔍 daemon".to_string(),
        format!("找到 {} 個相關段落：", rows.len()),
    ];
    for r in &rows {
        let path = r["path"].as_str().unwrap_or("");
        let title = r["title"].as_str().unwrap_or(path);
        let section: String = r["section"].as_str().unwrap_or("").chars().take(200).collect();
        lines.push(format!("- **{}** ({})\n  {}…", title, path, section.trim()));
    }
    Ok(lines.join("\n"))
}

/// 回傳目前 vault 的 UUID（前端用於 daemon REST API 呼叫）
#[tauri::command]
pub async fn get_vault_uuid(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.get_vault_uuid().await)
}

/// 設定筆記的 frontmatter status 欄位（draft / verified / deprecated）
#[tauri::command]
pub async fn set_note_status(
    state: State<'_, AppState>,
    path: String,
    status: String,
) -> Result<(), AppError> {
    if !matches!(status.as_str(), "draft" | "verified" | "deprecated") {
        return Err(AppError::AI(format!("Invalid status: {}", status)));
    }
    let vault_id = state.get_vault_id().await?;
    let vault_path = state.get_vault_path().await;

    let abs = if !vault_path.is_empty() {
        Some(std::path::Path::new(&vault_path).join(&path))
    } else {
        None
    };

    let on_disk = abs.as_ref().map(|p| p.exists()).unwrap_or(false);

    let _new_content: String = if on_disk {
        let abs_path = abs.as_ref().unwrap();
        let content = tokio::fs::read_to_string(abs_path).await
            .map_err(|e| AppError::AI(format!("Read failed: {}", e)))?;
        let updated = crate::runtime::tool_dispatch::set_frontmatter_key(&content, "status", &status);
        tokio::fs::write(abs_path, &updated).await
            .map_err(|e| AppError::AI(format!("Write failed: {}", e)))?;
        {
            let token = state.get_auth_token().await;
            let tok: Option<&str> = if token.is_empty() { None } else { Some(token.as_str()) };
            let _ = crate::api_client::daemon_post::<_, serde_json::Value>(
                &state.http_client,
                &format!("/vaults/{}/notes", urlencoding::encode(&vault_id)),
                &serde_json::json!({"path": path, "content": updated.clone()}),
                tok,
            ).await;
        }
        updated
    } else {
        format!("---\nstatus: {}\n---\n\n", status)
    };
    Ok(())
}
