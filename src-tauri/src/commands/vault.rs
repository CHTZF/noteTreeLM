use crate::{
    error::AppError,
    state::AppState,
    vault::{extract_title, count_words, indexer},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::path::PathBuf;
use tauri::State;

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
    pub id: i64,
    pub source_path: String,
    pub target_title: String,
    pub target_path: Option<String>,
    pub link_type: String,
    pub raw_text: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    pub line_number: i64,
}

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
    let db = state.get_vault_db().await?;

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

    // 檢查是否已有同名筆記
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT path FROM notes WHERE title = ?"
    )
    .bind(&title)
    .fetch_optional(&db)
    .await?;

    if existing.is_some() {
        return Err(AppError::Vault(format!(
            "已存在同名筆記：「{}」，請使用其他名稱或不同資料夾。",
            title
        )));
    }

    let abs_path = PathBuf::from(&vault_path).join(&rel_path);
    if let Some(parent) = abs_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(&abs_path, &content).await?;

    let now = chrono::Utc::now().timestamp_millis();
    let checksum = compute_checksum(&content);
    let word_count = count_words(&content);

    sqlx::query(
        "INSERT INTO notes(path, title, content, word_count, created_at, modified_at, checksum)
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&rel_path)
    .bind(&title)
    .bind(&content)
    .bind(word_count)
    .bind(now)
    .bind(now)
    .bind(&checksum)
    .execute(&db)
    .await?;

    // 同步 graph_nodes
    sqlx::query(
        "INSERT OR REPLACE INTO graph_nodes(id, node_type, label, created_at)
         VALUES (?, 'note', ?, ?)"
    )
    .bind(&rel_path)
    .bind(&title)
    .bind(now / 1000)
    .execute(&db)
    .await?;

    Ok(Note {
        path: rel_path,
        title,
        content,
        frontmatter: None,
        word_count,
        created_at: now,
        modified_at: now,
    })
}

#[tauri::command]
pub async fn read_note(
    state: State<'_, AppState>,
    path: String,
) -> Result<Note, AppError> {
    let db = state.get_vault_db().await?;
    let row = sqlx::query_as::<_, (String, String, String, Option<String>, i64, i64, i64)>(
        "SELECT path, title, content, frontmatter, word_count, created_at, modified_at
         FROM notes WHERE path = ?"
    )
    .bind(&path)
    .fetch_optional(&db)
    .await?
    .ok_or_else(|| AppError::Vault(format!("找不到筆記：{}", path)))?;

    Ok(Note {
        path: row.0,
        title: row.1,
        content: row.2,
        frontmatter: row.3,
        word_count: row.4,
        created_at: row.5,
        modified_at: row.6,
    })
}

#[tauri::command]
pub async fn update_note(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.get_vault_db().await?;
    let abs_path = PathBuf::from(&vault_path).join(&path);

    tokio::fs::write(&abs_path, &content).await?;

    let now = chrono::Utc::now().timestamp_millis();
    let checksum = compute_checksum(&content);
    let title = extract_title(&path, &content);
    let word_count = count_words(&content);

    sqlx::query(
        "UPDATE notes SET content = ?, title = ?, word_count = ?, modified_at = ?, checksum = ?
         WHERE path = ?"
    )
    .bind(&content)
    .bind(&title)
    .bind(word_count)
    .bind(now)
    .bind(&checksum)
    .bind(&path)
    .execute(&db)
    .await?;

    // 更新 links
    sync_links(&db, &path, &content).await?;

    // 更新 graph node label
    sqlx::query("UPDATE graph_nodes SET label = ? WHERE id = ?")
        .bind(&title)
        .bind(&path)
        .execute(&db)
        .await?;

    Ok(())
}

async fn sync_links(
    db: &SqlitePool,
    source_path: &str,
    content: &str,
) -> Result<(), AppError> {
    // 刪除舊的 links（wikilink 和 image_embed）
    sqlx::query("DELETE FROM links WHERE source_path = ?")
        .bind(source_path)
        .execute(db)
        .await?;

    // 解析並插入新 links
    let parsed = indexer::parse_links(content);
    for link in parsed {
        // 嘗試解析 target_path
        let target_path: Option<String> = sqlx::query_scalar(
            "SELECT path FROM notes WHERE title = ? LIMIT 1"
        )
        .bind(&link.target_title)
        .fetch_optional(db)
        .await?;

        sqlx::query(
            "INSERT OR IGNORE INTO links(source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(source_path)
        .bind(&link.target_title)
        .bind(&target_path)
        .bind(&link.link_type)
        .bind(&link.raw_text)
        .bind(&link.alias)
        .bind(&link.heading)
        .bind(link.line_number)
        .execute(db)
        .await?;

        // 更新 graph edge（僅 wikilink）
        if link.link_type == "wikilink" {
            if let Some(ref tp) = target_path {
                sqlx::query(
                    "INSERT OR IGNORE INTO graph_edges(source_id, target_id, edge_type)
                     VALUES (?, ?, 'wikilink')"
                )
                .bind(source_path)
                .bind(tp)
                .execute(db)
                .await?;
            }
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_note(
    state: State<'_, AppState>,
    path: String,
) -> Result<DeleteResult, AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.get_vault_db().await?;

    // 計算反向連結數量
    let affected_links: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM links WHERE target_path = ?"
    )
    .bind(&path)
    .fetch_one(&db)
    .await?;

    // 刪除實體檔案
    let abs_path = PathBuf::from(&vault_path).join(&path);
    tokio::fs::remove_file(&abs_path).await.ok();

    // DB 刪除（CASCADE 自動清除 links、tags、graph_nodes、graph_edges）
    sqlx::query("DELETE FROM notes WHERE path = ?")
        .bind(&path)
        .execute(&db)
        .await?;
    sqlx::query("DELETE FROM graph_nodes WHERE id = ?")
        .bind(&path)
        .execute(&db)
        .await?;

    Ok(DeleteResult { affected_links })
}

#[tauri::command]
pub async fn rename_note(
    state: State<'_, AppState>,
    path: String,
    new_title: String,
) -> Result<RenameResult, AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.get_vault_db().await?;

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

    // 找出所有引用舊標題的筆記
    let old_title: String = sqlx::query_scalar("SELECT title FROM notes WHERE path = ?")
        .bind(&path)
        .fetch_one(&db)
        .await?;

    let backlinks: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT source_path FROM links WHERE target_title = ?"
    )
    .bind(&old_title)
    .fetch_all(&db)
    .await?;

    // 更新每個反向連結的檔案內容
    let mut updated_files = Vec::new();
    for source_path in &backlinks {
        let abs_source = PathBuf::from(&vault_path).join(source_path);
        if let Ok(source_content) = tokio::fs::read_to_string(&abs_source).await {
            let updated = source_content
                .replace(&format!("[[{}]]", old_title), &format!("[[{}]]", new_title))
                .replace(&format!("[[{}|", old_title), &format!("[[{}|", new_title))
                .replace(&format!("[[{}#", old_title), &format!("[[{}#", new_title));

            if updated != source_content {
                tokio::fs::write(&abs_source, &updated).await?;
                updated_files.push(source_path.clone());
            }
        }
    }

    // 重新命名實體檔案
    let abs_old = PathBuf::from(&vault_path).join(&path);
    let abs_new = PathBuf::from(&vault_path).join(&new_path);
    if let Some(parent) = abs_new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&abs_old, &abs_new).await?;

    // 更新 DB（ON UPDATE CASCADE 會自動更新 links.source_path 和 links.target_path）
    sqlx::query("UPDATE notes SET path = ?, title = ? WHERE path = ?")
        .bind(&new_path)
        .bind(&new_title)
        .bind(&path)
        .execute(&db)
        .await?;
    sqlx::query("UPDATE graph_nodes SET id = ?, label = ? WHERE id = ?")
        .bind(&new_path)
        .bind(&new_title)
        .bind(&path)
        .execute(&db)
        .await?;

    Ok(RenameResult { new_path, updated_files })
}

#[tauri::command]
pub async fn list_notes(
    state: State<'_, AppState>,
    folder: Option<String>,
) -> Result<Vec<Note>, AppError> {
    let db = state.get_vault_db().await?;
    let rows = if let Some(f) = folder {
        let prefix = format!("{}/", f.trim_end_matches('/'));
        sqlx::query_as::<_, (String, String, String, Option<String>, i64, i64, i64)>(
            "SELECT path, title, content, frontmatter, word_count, created_at, modified_at
             FROM notes WHERE path LIKE ? ORDER BY modified_at DESC"
        )
        .bind(format!("{}%", prefix))
        .fetch_all(&db)
        .await?
    } else {
        sqlx::query_as::<_, (String, String, String, Option<String>, i64, i64, i64)>(
            "SELECT path, title, content, frontmatter, word_count, created_at, modified_at
             FROM notes ORDER BY modified_at DESC"
        )
        .fetch_all(&db)
        .await?
    };

    Ok(rows
        .into_iter()
        .map(|r| Note {
            path: r.0,
            title: r.1,
            content: r.2,
            frontmatter: r.3,
            word_count: r.4,
            created_at: r.5,
            modified_at: r.6,
        })
        .collect())
}

#[tauri::command]
pub async fn get_backlinks(
    state: State<'_, AppState>,
    title: String,
) -> Result<Vec<Link>, AppError> {
    let db = state.get_vault_db().await?;
    let rows = sqlx::query_as::<_, (i64, String, String, Option<String>, String, String, Option<String>, Option<String>, i64)>(
        "SELECT id, source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number
         FROM links WHERE target_title = ? ORDER BY source_path"
    )
    .bind(&title)
    .fetch_all(&db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| Link {
            id: r.0,
            source_path: r.1,
            target_title: r.2,
            target_path: r.3,
            link_type: r.4,
            raw_text: r.5,
            alias: r.6,
            heading: r.7,
            line_number: r.8,
        })
        .collect())
}

/// 掃描整個 Vault，建立或更新所有筆記的索引
#[tauri::command]
pub async fn scan_vault(state: State<'_, AppState>) -> Result<usize, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    let db = state.get_vault_db().await?;

    let mut count = 0;
    scan_dir(&db, &vault_path, &vault_path, &mut count).await?;

    // 清除 DB 中已不存在於磁碟的幽靈條目
    let all_db_paths: Vec<String> = sqlx::query_scalar("SELECT path FROM notes")
        .fetch_all(&db)
        .await?;
    for path in all_db_paths {
        let abs = PathBuf::from(&vault_path).join(&path);
        if !abs.exists() {
            sqlx::query("DELETE FROM notes WHERE path = ?")
                .bind(&path)
                .execute(&db)
                .await?;
            sqlx::query("DELETE FROM graph_nodes WHERE id = ?")
                .bind(&path)
                .execute(&db)
                .await?;
        }
    }
    // 清除 graph_nodes 中孤立的 note 節點
    sqlx::query(
        "DELETE FROM graph_nodes WHERE node_type = 'note' AND id NOT IN (SELECT path FROM notes)"
    )
    .execute(&db)
    .await?;

    Ok(count)
}

/// 移動筆記到不同資料夾（保持檔案名稱與標題不變）
#[tauri::command]
pub async fn move_note(
    state: State<'_, AppState>,
    old_path: String,
    new_folder: String,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.get_vault_db().await?;

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

    // notes.path 更新（links 有 ON UPDATE CASCADE 自動跟著更新）
    sqlx::query("UPDATE notes SET path = ? WHERE path = ?")
        .bind(&new_path).bind(&old_path).execute(&db).await?;

    // graph_nodes / graph_edges 手動更新
    sqlx::query("UPDATE graph_nodes SET id = ? WHERE id = ?")
        .bind(&new_path).bind(&old_path).execute(&db).await?;
    sqlx::query("UPDATE graph_edges SET source_id = ? WHERE source_id = ?")
        .bind(&new_path).bind(&old_path).execute(&db).await?;
    sqlx::query("UPDATE graph_edges SET target_id = ? WHERE target_id = ?")
        .bind(&new_path).bind(&old_path).execute(&db).await?;

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
    let db = state.get_vault_db().await?;
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

    // 更新 DB 中所有路徑前綴符合的筆記
    let old_prefix = format!("{}/", folder_path);
    let new_prefix = format!("{}/", new_folder_path);
    let note_paths: Vec<String> =
        sqlx::query_scalar("SELECT path FROM notes WHERE path LIKE ?")
            .bind(format!("{}%", old_prefix))
            .fetch_all(&db)
            .await?;

    for old_note_path in &note_paths {
        let new_note_path = format!(
            "{}{}",
            new_prefix,
            &old_note_path[old_prefix.len()..]
        );
        sqlx::query("UPDATE notes SET path = ? WHERE path = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
        sqlx::query("UPDATE graph_nodes SET id = ? WHERE id = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
        sqlx::query("UPDATE graph_edges SET source_id = ? WHERE source_id = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
        sqlx::query("UPDATE graph_edges SET target_id = ? WHERE target_id = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
    }

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
    let db = state.get_vault_db().await?;
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

    // 更新 DB 中所有路徑前綴符合的筆記
    let old_prefix = format!("{}/", folder_path);
    let new_prefix = format!("{}/", new_folder_path);
    let note_paths: Vec<String> =
        sqlx::query_scalar("SELECT path FROM notes WHERE path LIKE ?")
            .bind(format!("{}%", old_prefix))
            .fetch_all(&db)
            .await?;

    for old_note_path in &note_paths {
        let new_note_path = format!("{}{}", new_prefix, &old_note_path[old_prefix.len()..]);
        sqlx::query("UPDATE notes SET path = ? WHERE path = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
        sqlx::query("UPDATE graph_nodes SET id = ? WHERE id = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
        sqlx::query("UPDATE graph_edges SET source_id = ? WHERE source_id = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
        sqlx::query("UPDATE graph_edges SET target_id = ? WHERE target_id = ?")
            .bind(&new_note_path)
            .bind(old_note_path)
            .execute(&db)
            .await?;
    }

    Ok(new_folder_path)
}

/// 相對路徑由前端以 vault_path 補完後傳入
#[tauri::command]
pub async fn read_file_base64(path: String) -> Result<String, AppError> {
    let bytes = std::fs::read(&path)
        .map_err(|e| AppError::Vault(format!("無法讀取圖片 {}: {}", path, e)))?;
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
    let db = state.get_vault_db().await?;
    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }

    // 取得此資料夾下的所有筆記路徑
    let prefix = format!("{}/", folder_path.trim_end_matches('/'));
    let note_paths: Vec<String> = sqlx::query_scalar(
        "SELECT path FROM notes WHERE path LIKE ?"
    )
    .bind(format!("{}%", prefix))
    .fetch_all(&db)
    .await?;

    // 從 DB 刪除所有筆記（CASCADE 自動清除 links）
    for path in &note_paths {
        sqlx::query("DELETE FROM notes WHERE path = ?")
            .bind(path).execute(&db).await?;
        sqlx::query("DELETE FROM graph_nodes WHERE id = ?")
            .bind(path).execute(&db).await?;
    }

    // 刪除實體目錄（遞迴）
    let abs_path = PathBuf::from(&vault_path).join(&folder_path);
    tokio::fs::remove_dir_all(&abs_path).await
        .map_err(|e| AppError::Vault(format!("刪除資料夾失敗：{}", e)))?;

    Ok(note_paths.len() as u32)
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"];

/// 將圖片檔案複製到 Vault（指定資料夾，預設根目錄）
#[tauri::command]
pub async fn import_image(
    state: State<'_, AppState>,
    source_path: String,
    folder: Option<String>,
) -> Result<String, AppError> {
    let vault_path = state.get_vault_path().await;
    if vault_path.is_empty() {
        return Err(AppError::Vault("尚未設定 Vault 路徑".to_string()));
    }
    let filename = PathBuf::from(&source_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| AppError::Vault("無效的圖片路徑".to_string()))?;
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
            if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                if let Some(rel) = crate::vault::to_relative_path(vault_root, &path) {
                    assets.push(rel);
                }
            }
        }
    }
    Ok(())
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

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct TrashItem {
    pub id: String,
    pub original_path: String,
    pub name: String,
    pub title: String,
    pub trash_filename: String,
    pub deleted_at: i64,
}

/// 將單一筆記移入 .trash/ 目錄（內部輔助函式）
async fn trash_single_note(
    db: &SqlitePool,
    vault_path: &str,
    note_path: &str,
) -> Result<(), AppError> {
    let title: String = sqlx::query_scalar("SELECT title FROM notes WHERE path = ?")
        .bind(note_path)
        .fetch_optional(db)
        .await?
        .unwrap_or_default();

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

    let abs_path = PathBuf::from(vault_path).join(note_path);
    if abs_path.exists() {
        tokio::fs::rename(&abs_path, trash_dir.join(&trash_filename))
            .await
            .map_err(|e| AppError::Vault(format!("移動到垃圾桶失敗：{}", e)))?;
    }

    // 從 DB 刪除（CASCADE 自動清除 links、tags）
    sqlx::query("DELETE FROM notes WHERE path = ?")
        .bind(note_path)
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM graph_nodes WHERE id = ?")
        .bind(note_path)
        .execute(db)
        .await?;

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO trash_items(id, original_path, name, title, trash_filename, deleted_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(note_path)
    .bind(&filename)
    .bind(&title)
    .bind(&trash_filename)
    .bind(now)
    .execute(db)
    .await?;

    Ok(())
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
    let db = state.get_vault_db().await?;
    trash_single_note(&db, &vault_path, &path).await
}

/// 將資料夾中所有筆記移至垃圾桶，然後刪除實體資料夾
#[tauri::command]
pub async fn trash_folder(
    state: State<'_, AppState>,
    folder_path: String,
) -> Result<u32, AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.get_vault_db().await?;
    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }

    let prefix = format!("{}/", folder_path.trim_end_matches('/'));
    let note_paths: Vec<String> =
        sqlx::query_scalar("SELECT path FROM notes WHERE path LIKE ?")
            .bind(format!("{}%", prefix))
            .fetch_all(&db)
            .await?;

    let count = note_paths.len() as u32;
    for note_path in &note_paths {
        trash_single_note(&db, &vault_path, note_path).await?;
    }

    let abs_path = PathBuf::from(&vault_path).join(&folder_path);
    if abs_path.exists() {
        tokio::fs::remove_dir_all(&abs_path)
            .await
            .map_err(|e| AppError::Vault(format!("刪除資料夾失敗：{}", e)))?;
    }

    Ok(count)
}

/// 列出垃圾桶中所有項目（依刪除時間降序）
#[tauri::command]
pub async fn list_trash(
    state: State<'_, AppState>,
) -> Result<Vec<TrashItem>, AppError> {
    let db = state.get_vault_db().await?;
    let items: Vec<TrashItem> = sqlx::query_as(
        "SELECT id, original_path, name, title, trash_filename, deleted_at
         FROM trash_items ORDER BY deleted_at DESC",
    )
    .fetch_all(&db)
    .await?;
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
    let db = state.get_vault_db().await?;

    let item: Option<TrashItem> = sqlx::query_as(
        "SELECT id, original_path, name, title, trash_filename, deleted_at
         FROM trash_items WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&db)
    .await?;
    let item = item.ok_or_else(|| AppError::Vault("找不到垃圾桶項目".to_string()))?;

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
    let content = tokio::fs::read_to_string(&trash_file)
        .await
        .map_err(|e| AppError::Vault(format!("無法讀取垃圾桶檔案：{}", e)))?;

    let abs_new = PathBuf::from(&vault_path).join(&new_path);
    if let Some(parent) = abs_new.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(&trash_file, &abs_new)
        .await
        .map_err(|e| AppError::Vault(format!("復原失敗：{}", e)))?;

    // 重新建立 DB 索引
    let title = extract_title(&new_path, &content);
    let now = chrono::Utc::now().timestamp_millis();
    let word_count = count_words(&content);
    let checksum = {
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        format!("{:x}", h.finalize())
    };

    sqlx::query(
        "INSERT INTO notes(path, title, content, word_count, created_at, modified_at, checksum)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(path) DO UPDATE SET
           title = excluded.title, content = excluded.content,
           word_count = excluded.word_count, modified_at = excluded.modified_at,
           checksum = excluded.checksum",
    )
    .bind(&new_path).bind(&title).bind(&content)
    .bind(word_count).bind(now).bind(now).bind(&checksum)
    .execute(&db).await?;

    sqlx::query(
        "INSERT OR REPLACE INTO graph_nodes(id, node_type, label, created_at)
         VALUES (?, 'note', ?, ?)",
    )
    .bind(&new_path).bind(&title).bind(now / 1000)
    .execute(&db).await?;

    sqlx::query("DELETE FROM links WHERE source_path = ?")
        .bind(&new_path).execute(&db).await?;
    let parsed_links = indexer::parse_links(&content);
    for link in parsed_links {
        let target_path: Option<String> =
            sqlx::query_scalar("SELECT path FROM notes WHERE title = ? LIMIT 1")
                .bind(&link.target_title)
                .fetch_optional(&db)
                .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO links(source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&new_path).bind(&link.target_title).bind(&target_path)
        .bind(link.link_type.as_str()).bind(&link.raw_text)
        .bind(&link.alias).bind(&link.heading).bind(link.line_number)
        .execute(&db).await?;
    }

    sqlx::query("DELETE FROM trash_items WHERE id = ?")
        .bind(&id).execute(&db).await?;

    Ok(new_path)
}

/// 徹底刪除垃圾桶中的項目（不可復原）
#[tauri::command]
pub async fn delete_trash_items(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.get_vault_db().await?;
    let trash_dir = PathBuf::from(&vault_path).join(".trash");

    for id in &ids {
        let trash_filename: Option<String> =
            sqlx::query_scalar("SELECT trash_filename FROM trash_items WHERE id = ?")
                .bind(id)
                .fetch_optional(&db)
                .await?;

        if let Some(filename) = trash_filename {
            let file_path = trash_dir.join(&filename);
            if file_path.exists() {
                let _ = tokio::fs::remove_file(&file_path).await;
            }
        }
        sqlx::query("DELETE FROM trash_items WHERE id = ?")
            .bind(id).execute(&db).await?;
    }

    Ok(())
}

async fn scan_dir(
    db: &SqlitePool,
    vault_root: &str,
    dir: &str,
    count: &mut usize,
) -> Result<(), AppError> {
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

        // 跳過隱藏目錄和 assets
        if name.starts_with('.') || name == "assets" {
            continue;
        }

        if path.is_dir() {
            Box::pin(scan_dir(db, vault_root, &path.to_string_lossy(), count)).await?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            let rel_path = crate::vault::to_relative_path(vault_root, &path)
                .unwrap_or_else(|| name.clone());
            let title = extract_title(&rel_path, &content);
            let checksum = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(content.as_bytes());
                format!("{:x}", h.finalize())
            };

            // 檢查是否已存在且 checksum 相同（未變動則跳過）
            let existing_checksum: Option<String> = sqlx::query_scalar(
                "SELECT checksum FROM notes WHERE path = ?"
            )
            .bind(&rel_path)
            .fetch_optional(db)
            .await?;

            if existing_checksum.as_deref() == Some(&checksum) {
                continue;
            }

            let now = chrono::Utc::now().timestamp_millis();
            let word_count = crate::vault::count_words(&content);

            sqlx::query(
                "INSERT INTO notes(path, title, content, word_count, created_at, modified_at, checksum)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET
                   title = excluded.title,
                   content = excluded.content,
                   word_count = excluded.word_count,
                   modified_at = excluded.modified_at,
                   checksum = excluded.checksum"
            )
            .bind(&rel_path)
            .bind(&title)
            .bind(&content)
            .bind(word_count)
            .bind(now)
            .bind(now)
            .bind(&checksum)
            .execute(db)
            .await?;

            // 更新 graph node
            sqlx::query(
                "INSERT OR REPLACE INTO graph_nodes(id, node_type, label, created_at)
                 VALUES (?, 'note', ?, ?)"
            )
            .bind(&rel_path)
            .bind(&title)
            .bind(now / 1000)
            .execute(db)
            .await?;

            // 重新解析 links
            sqlx::query("DELETE FROM links WHERE source_path = ?")
                .bind(&rel_path)
                .execute(db)
                .await?;

            let parsed_links = crate::vault::indexer::parse_links(&content);
            for link in parsed_links {
                let target_path: Option<String> = sqlx::query_scalar(
                    "SELECT path FROM notes WHERE title = ? LIMIT 1"
                )
                .bind(&link.target_title)
                .fetch_optional(db)
                .await?;

                sqlx::query(
                    "INSERT OR IGNORE INTO links(source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                )
                .bind(&rel_path)
                .bind(&link.target_title)
                .bind(&target_path)
                .bind(&link.link_type)
                .bind(&link.raw_text)
                .bind(&link.alias)
                .bind(&link.heading)
                .bind(link.line_number)
                .execute(db)
                .await?;
            }

            *count += 1;
        }
    }
    Ok(())
}
