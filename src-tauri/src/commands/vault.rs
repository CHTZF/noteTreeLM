use crate::{
    error::AppError,
    state::AppState,
    vault::{extract_title, count_words, indexer, chunker},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tauri::State;

/// 若 embedding server 正在運行，回傳其 base URL；否則回傳 None。
async fn embedding_url(state: &AppState) -> Option<String> {
    let port = *state.embedding_actual_port.lock().await;
    port.map(|p| format!("http://127.0.0.1:{}", p))
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

fn compute_checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Internal row for deserializing note fields from SurrealDB.
/// created_at / modified_at are stored as SurrealDB datetime, we convert to ms.
#[derive(Deserialize)]
struct NoteRow {
    path: String,
    title: String,
    content: String,
    frontmatter: Option<String>,
    word_count: i64,
    created_at: surrealdb::sql::Datetime,
    modified_at: surrealdb::sql::Datetime,
}

impl From<NoteRow> for Note {
    fn from(r: NoteRow) -> Self {
        Note {
            path: r.path,
            title: r.title,
            content: r.content,
            frontmatter: r.frontmatter,
            word_count: r.word_count,
            created_at: r.created_at.timestamp_millis(),
            modified_at: r.modified_at.timestamp_millis(),
        }
    }
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

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
    #[derive(Deserialize)]
    struct PathRow { path: String }
    let mut resp = db
        .query("SELECT path FROM notes WHERE title = $title AND vault_id = $vid LIMIT 1")
        .bind(("title", title.clone()))
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let existing_rows: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    if !existing_rows.is_empty() {
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

    let now_ms = chrono::Utc::now().timestamp_millis();
    let checksum = compute_checksum(&content);
    let word_count = count_words(&content);

    // Store timestamps as SurrealDB datetime using microsecond-precision ISO string
    let now_dt = surrealdb::sql::Datetime::from(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
    );

    db.query(
        "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum)
         VALUES ($vid, $path, $title, $content, $wc, $now, $now, $cs)"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", rel_path.clone()))
    .bind(("title", title.clone()))
    .bind(("content", content.clone()))
    .bind(("wc", word_count))
    .bind(("now", now_dt.clone()))
    .bind(("cs", checksum.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    // 同步 graph_nodes
    db.query(
        "INSERT INTO graph_nodes (vault_id, node_id, node_type, label)
         VALUES ($vid, $path, 'note', $title)
         ON DUPLICATE KEY UPDATE label = $title"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", rel_path.clone()))
    .bind(("title", title.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    // 建立 chunks（忽略失敗，不影響主流程）
    let chunks = chunker::chunk_note(&rel_path, &content, now_ms);
    let emb_url = embedding_url(&state).await;
    let _ = chunker::upsert_chunks(&db, &vault_id, &chunks, emb_url.as_deref()).await;

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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    let mut resp = db
        .query("SELECT path, title, content, frontmatter, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<NoteRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Vault(format!("找不到筆記：{}", path)))?;

    Ok(row.into())
}

#[tauri::command]
pub async fn update_note(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;
    let abs_path = PathBuf::from(&vault_path).join(&path);

    tokio::fs::write(&abs_path, &content).await?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let checksum = compute_checksum(&content);
    let title = extract_title(&path, &content);
    let word_count = count_words(&content);

    let now_dt = surrealdb::sql::Datetime::from(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
    );

    db.query(
        "UPDATE notes SET content = $content, title = $title, word_count = $wc, modified_at = $now, checksum = $cs
         WHERE vault_id = $vid AND path = $path"
    )
    .bind(("content", content.clone()))
    .bind(("title", title.clone()))
    .bind(("wc", word_count))
    .bind(("now", now_dt))
    .bind(("cs", checksum.clone()))
    .bind(("vid", vault_id.clone()))
    .bind(("path", path.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    // 更新 links
    sync_links(&db, &vault_id, &path, &content).await?;

    // 更新 graph node label
    db.query("UPDATE graph_nodes SET label = $title WHERE vault_id = $vid AND node_id = $path")
        .bind(("title", title.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    // 更新 chunks
    let chunks = chunker::chunk_note(&path, &content, now_ms);
    let emb_url = embedding_url(&state).await;
    let _ = chunker::upsert_chunks(&db, &vault_id, &chunks, emb_url.as_deref()).await;

    Ok(())
}

async fn sync_links(
    db: &crate::db::surreal::SurrealDb,
    vault_id: &str,
    source_path: &str,
    content: &str,
) -> Result<(), AppError> {
    // 刪除舊的 links（wikilink 和 image_embed）
    db.query("DELETE FROM links WHERE vault_id = $vid AND source_path = $sp")
        .bind(("vid", vault_id.to_owned()))
        .bind(("sp", source_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    // 解析並插入新 links
    let parsed = indexer::parse_links(content);
    for link in parsed {
        // 嘗試解析 target_path
        #[derive(Deserialize)]
        struct PathRow { path: String }
        let mut resp = db
            .query("SELECT path FROM notes WHERE vault_id = $vid AND title = $title LIMIT 1")
            .bind(("vid", vault_id.to_owned()))
            .bind(("title", link.target_title.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let path_rows: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
        let target_path: Option<String> = path_rows.into_iter().next().map(|r| r.path);

        db.query(
            "INSERT INTO links (vault_id, source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number)
             VALUES ($vid, $sp, $tt, $tp, $lt, $rt, $alias, $heading, $ln)
             ON DUPLICATE KEY UPDATE source_path = $sp"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("sp", source_path.to_owned()))
        .bind(("tt", link.target_title.clone()))
        .bind(("tp", target_path.clone()))
        .bind(("lt", link.link_type.clone()))
        .bind(("rt", link.raw_text.clone()))
        .bind(("alias", link.alias.clone()))
        .bind(("heading", link.heading.clone()))
        .bind(("ln", link.line_number))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

        // 更新 graph edge（僅 wikilink）
        if link.link_type == "wikilink" {
            if let Some(ref tp) = target_path {
                db.query(
                    "INSERT INTO graph_edges (vault_id, source_id, target_id, edge_type)
                     VALUES ($vid, $src, $tgt, 'wikilink')
                     ON DUPLICATE KEY UPDATE edge_type = 'wikilink'"
                )
                .bind(("vid", vault_id.to_owned()))
                .bind(("src", source_path.to_owned()))
                .bind(("tgt", tp.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    // 計算反向連結數量
    #[derive(Deserialize)]
    struct CountRow { count: i64 }
    let mut resp = db
        .query("SELECT count() AS count FROM links WHERE vault_id = $vid AND target_path = $path GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let count_rows: Vec<CountRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let affected_links: i64 = count_rows.into_iter().next().map(|r| r.count).unwrap_or(0);

    // 刪除實體檔案
    let abs_path = PathBuf::from(&vault_path).join(&path);
    tokio::fs::remove_file(&abs_path).await.ok();

    // DB 刪除
    db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("DELETE FROM graph_nodes WHERE vault_id = $vid AND node_id = $path")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("DELETE FROM links WHERE vault_id = $vid AND source_path = $path")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    // 刪除對應 chunks
    let _ = chunker::delete_chunks(&db, &vault_id, &path).await;

    Ok(DeleteResult { affected_links })
}

#[tauri::command]
pub async fn rename_note(
    state: State<'_, AppState>,
    path: String,
    new_title: String,
) -> Result<RenameResult, AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

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
    #[derive(Deserialize)]
    struct TitleRow { title: String }
    let mut resp = db
        .query("SELECT title FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let title_rows: Vec<TitleRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let old_title = title_rows
        .into_iter()
        .next()
        .map(|r| r.title)
        .ok_or_else(|| AppError::Vault(format!("找不到筆記：{}", path)))?;

    #[derive(Deserialize)]
    struct SourcePathRow { source_path: String }
    let mut resp = db
        .query("SELECT source_path FROM links WHERE vault_id = $vid AND target_title = $title")
        .bind(("vid", vault_id.clone()))
        .bind(("title", old_title.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let backlink_rows: Vec<SourcePathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let backlinks: Vec<String> = backlink_rows.into_iter().map(|r| r.source_path).collect();

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

    // 更新 DB
    db.query("UPDATE notes SET path = $new_path, title = $new_title WHERE vault_id = $vid AND path = $path")
        .bind(("new_path", new_path.clone()))
        .bind(("new_title", new_title.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("UPDATE graph_nodes SET node_id = $new_path, label = $new_title WHERE vault_id = $vid AND node_id = $path")
        .bind(("new_path", new_path.clone()))
        .bind(("new_title", new_title.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("path", path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(RenameResult { new_path, updated_files })
}

#[tauri::command]
pub async fn list_notes(
    state: State<'_, AppState>,
    folder: Option<String>,
) -> Result<Vec<Note>, AppError> {
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    let rows: Vec<NoteRow> = if let Some(f) = folder {
        let prefix = format!("{}/", f.trim_end_matches('/'));
        let mut resp = db
            .query("SELECT path, title, content, frontmatter, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix) ORDER BY modified_at DESC")
            .bind(("vid", vault_id.clone()))
            .bind(("prefix", prefix.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        resp.take(0).map_err(|e| AppError::Database(e.to_string()))?
    } else {
        let mut resp = db
            .query("SELECT path, title, content, frontmatter, word_count, created_at, modified_at FROM notes WHERE vault_id = $vid ORDER BY modified_at DESC")
            .bind(("vid", vault_id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        resp.take(0).map_err(|e| AppError::Database(e.to_string()))?
    };

    Ok(rows.into_iter().map(|r| r.into()).collect())
}

#[tauri::command]
pub async fn get_backlinks(
    state: State<'_, AppState>,
    title: String,
) -> Result<Vec<Link>, AppError> {
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct LinkRow {
        id: surrealdb::sql::Thing,
        source_path: String,
        target_title: String,
        target_path: Option<String>,
        link_type: String,
        raw_text: String,
        alias: Option<String>,
        heading: Option<String>,
        line_number: i64,
    }

    let mut resp = db
        .query("SELECT id, source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number FROM links WHERE vault_id = $vid AND target_title = $title ORDER BY source_path")
        .bind(("vid", vault_id.clone()))
        .bind(("title", title.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<LinkRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    Ok(rows
        .into_iter()
        .map(|r| Link {
            id: r.id.to_string(),
            source_path: r.source_path,
            target_title: r.target_title,
            target_path: r.target_path,
            link_type: r.link_type,
            raw_text: r.raw_text,
            alias: r.alias,
            heading: r.heading,
            line_number: r.line_number,
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    let mut count = 0;
    scan_dir(&db, &vault_id, &vault_path, &vault_path, &mut count).await?;

    // 清除 DB 中已不存在於磁碟的幽靈條目
    #[derive(Deserialize)]
    struct PathRow { path: String }
    let mut resp = db
        .query("SELECT path FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let all_db_paths: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    for row in all_db_paths {
        let abs = PathBuf::from(&vault_path).join(&row.path);
        if !abs.exists() {
            db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
                .bind(("vid", vault_id.clone()))
                .bind(("path", row.path.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            db.query("DELETE FROM graph_nodes WHERE vault_id = $vid AND node_id = $path")
                .bind(("vid", vault_id.clone()))
                .bind(("path", row.path.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
        }
    }

    // 清除 graph_nodes 中孤立的 note 節點
    // 先取得所有 note 路徑再比對
    let mut resp2 = db
        .query("SELECT node_id FROM graph_nodes WHERE vault_id = $vid AND node_type = 'note'")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    #[derive(Deserialize)]
    struct NodeIdRow { node_id: String }
    let node_rows: Vec<NodeIdRow> = resp2.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    for node in node_rows {
        let mut check = db
            .query("SELECT path FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("path", node.node_id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        #[derive(Deserialize)]
        struct CheckRow { path: String }
        let check_rows: Vec<CheckRow> = check.take(0).map_err(|e| AppError::Database(e.to_string()))?;
        if check_rows.is_empty() {
            db.query("DELETE FROM graph_nodes WHERE vault_id = $vid AND node_id = $nid")
                .bind(("vid", vault_id.clone()))
                .bind(("nid", node.node_id.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
        }
    }

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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

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

    // notes.path 更新
    db.query("UPDATE notes SET path = $new_path WHERE vault_id = $vid AND path = $old_path")
        .bind(("new_path", new_path.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("old_path", old_path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    // graph_nodes / graph_edges 手動更新
    db.query("UPDATE graph_nodes SET node_id = $new_path WHERE vault_id = $vid AND node_id = $old_path")
        .bind(("new_path", new_path.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("old_path", old_path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("UPDATE graph_edges SET source_id = $new_path WHERE vault_id = $vid AND source_id = $old_path")
        .bind(("new_path", new_path.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("old_path", old_path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("UPDATE graph_edges SET target_id = $new_path WHERE vault_id = $vid AND target_id = $old_path")
        .bind(("new_path", new_path.clone()))
        .bind(("vid", vault_id.clone()))
        .bind(("old_path", old_path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

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

    #[derive(Deserialize)]
    struct PathRow { path: String }
    let mut resp = db
        .query("SELECT path FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)")
        .bind(("vid", vault_id.clone()))
        .bind(("prefix", old_prefix.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let note_paths: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    for row in &note_paths {
        let new_note_path = format!(
            "{}{}",
            new_prefix,
            &row.path[old_prefix.len()..]
        );
        db.query("UPDATE notes SET path = $new_path WHERE vault_id = $vid AND path = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("UPDATE graph_nodes SET node_id = $new_path WHERE vault_id = $vid AND node_id = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("UPDATE graph_edges SET source_id = $new_path WHERE vault_id = $vid AND source_id = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("UPDATE graph_edges SET target_id = $new_path WHERE vault_id = $vid AND target_id = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

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

    #[derive(Deserialize)]
    struct PathRow { path: String }
    let mut resp = db
        .query("SELECT path FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)")
        .bind(("vid", vault_id.clone()))
        .bind(("prefix", old_prefix.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let note_paths: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    for row in &note_paths {
        let new_note_path = format!("{}{}", new_prefix, &row.path[old_prefix.len()..]);
        db.query("UPDATE notes SET path = $new_path WHERE vault_id = $vid AND path = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("UPDATE graph_nodes SET node_id = $new_path WHERE vault_id = $vid AND node_id = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("UPDATE graph_edges SET source_id = $new_path WHERE vault_id = $vid AND source_id = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("UPDATE graph_edges SET target_id = $new_path WHERE vault_id = $vid AND target_id = $old_path")
            .bind(("new_path", new_note_path.clone()))
            .bind(("vid", vault_id.clone()))
            .bind(("old_path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }

    // 取得此資料夾下的所有筆記路徑
    let prefix = format!("{}/", folder_path.trim_end_matches('/'));

    #[derive(Deserialize)]
    struct PathRow { path: String }
    let mut resp = db
        .query("SELECT path FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)")
        .bind(("vid", vault_id.clone()))
        .bind(("prefix", prefix.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let note_paths: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    // 從 DB 刪除所有筆記
    for row in &note_paths {
        db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
            .bind(("vid", vault_id.clone()))
            .bind(("path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        db.query("DELETE FROM graph_nodes WHERE vault_id = $vid AND node_id = $path")
            .bind(("vid", vault_id.clone()))
            .bind(("path", row.path.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }

    // 刪除實體目錄（遞迴）
    let abs_path = PathBuf::from(&vault_path).join(&folder_path);
    tokio::fs::remove_dir_all(&abs_path).await
        .map_err(|e| AppError::Vault(format!("刪除資料夾失敗：{}", e)))?;

    Ok(note_paths.len() as u32)
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

#[derive(Deserialize)]
struct TrashItemRow {
    item_id: String,
    original_path: String,
    name: String,
    title: String,
    trash_filename: String,
    deleted_at: surrealdb::sql::Datetime,
}

impl From<TrashItemRow> for TrashItem {
    fn from(r: TrashItemRow) -> Self {
        TrashItem {
            id: r.item_id,
            original_path: r.original_path,
            name: r.name,
            title: r.title,
            trash_filename: r.trash_filename,
            deleted_at: r.deleted_at.timestamp_millis(),
        }
    }
}

/// 將單一筆記移入 .trash/ 目錄（內部輔助函式）
async fn trash_single_note(
    db: &crate::db::surreal::SurrealDb,
    vault_id: &str,
    vault_path: &str,
    note_path: &str,
) -> Result<(), AppError> {
    #[derive(Deserialize)]
    struct TitleRow { title: String }
    let mut resp = db
        .query("SELECT title FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
        .bind(("vid", vault_id.to_owned()))
        .bind(("path", note_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let title_rows: Vec<TitleRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let title = title_rows.into_iter().next().map(|r| r.title).unwrap_or_default();

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

    // 從 DB 刪除
    db.query("DELETE FROM notes WHERE vault_id = $vid AND path = $path")
        .bind(("vid", vault_id.to_owned()))
        .bind(("path", note_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    db.query("DELETE FROM graph_nodes WHERE vault_id = $vid AND node_id = $path")
        .bind(("vid", vault_id.to_owned()))
        .bind(("path", note_path.to_owned()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let item_id = uuid::Uuid::new_v4().to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_dt = surrealdb::sql::Datetime::from(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
    );

    db.query(
        "INSERT INTO trash_items (vault_id, item_id, original_path, name, title, trash_filename, deleted_at)
         VALUES ($vid, $item_id, $op, $name, $title, $tf, $deleted_at)"
    )
    .bind(("vid", vault_id.to_owned()))
    .bind(("item_id", item_id.clone()))
    .bind(("op", note_path.to_owned()))
    .bind(("name", filename.clone()))
    .bind(("title", title.clone()))
    .bind(("tf", trash_filename.clone()))
    .bind(("deleted_at", now_dt))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;
    trash_single_note(&db, &vault_id, &vault_path, &path).await
}

/// 將資料夾中所有筆記移至垃圾桶，然後刪除實體資料夾
#[tauri::command]
pub async fn trash_folder(
    state: State<'_, AppState>,
    folder_path: String,
) -> Result<u32, AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    if folder_path.contains("..") || folder_path.is_empty() {
        return Err(AppError::Vault("無效的資料夾路徑".to_string()));
    }

    let prefix = format!("{}/", folder_path.trim_end_matches('/'));

    #[derive(Deserialize)]
    struct PathRow { path: String }
    let mut resp = db
        .query("SELECT path FROM notes WHERE vault_id = $vid AND string::starts_with(path, $prefix)")
        .bind(("vid", vault_id.clone()))
        .bind(("prefix", prefix.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let note_paths: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    let count = note_paths.len() as u32;
    for row in &note_paths {
        trash_single_note(&db, &vault_id, &vault_path, &row.path).await?;
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    let mut resp = db
        .query("SELECT item_id, original_path, name, title, trash_filename, deleted_at FROM trash_items WHERE vault_id = $vid ORDER BY deleted_at DESC")
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<TrashItemRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    Ok(rows.into_iter().map(|r| r.into()).collect())
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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    let mut resp = db
        .query("SELECT item_id, original_path, name, title, trash_filename, deleted_at FROM trash_items WHERE vault_id = $vid AND item_id = $id LIMIT 1")
        .bind(("vid", vault_id.clone()))
        .bind(("id", id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let rows: Vec<TrashItemRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
    let item: TrashItem = rows
        .into_iter()
        .next()
        .map(|r| r.into())
        .ok_or_else(|| AppError::Vault("找不到垃圾桶項目".to_string()))?;

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
    let now_ms = chrono::Utc::now().timestamp_millis();
    let word_count = count_words(&content);
    let checksum = {
        let mut h = Sha256::new();
        h.update(content.as_bytes());
        format!("{:x}", h.finalize())
    };
    let now_dt = surrealdb::sql::Datetime::from(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
    );

    db.query(
        "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum)
         VALUES ($vid, $path, $title, $content, $wc, $now, $now, $cs)
         ON DUPLICATE KEY UPDATE
           title = $title, content = $content,
           word_count = $wc, modified_at = $now,
           checksum = $cs"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", new_path.clone()))
    .bind(("title", title.clone()))
    .bind(("content", content.clone()))
    .bind(("wc", word_count))
    .bind(("now", now_dt.clone()))
    .bind(("cs", checksum.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    db.query(
        "INSERT INTO graph_nodes (vault_id, node_id, node_type, label)
         VALUES ($vid, $path, 'note', $title)
         ON DUPLICATE KEY UPDATE label = $title"
    )
    .bind(("vid", vault_id.clone()))
    .bind(("path", new_path.clone()))
    .bind(("title", title.clone()))
    .await
    .map_err(|e| AppError::Database(e.to_string()))?;

    db.query("DELETE FROM links WHERE vault_id = $vid AND source_path = $path")
        .bind(("vid", vault_id.clone()))
        .bind(("path", new_path.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    let parsed_links = indexer::parse_links(&content);
    for link in parsed_links {
        #[derive(Deserialize)]
        struct PathRow { path: String }
        let mut resp = db
            .query("SELECT path FROM notes WHERE vault_id = $vid AND title = $title LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("title", link.target_title.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let path_rows: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
        let target_path: Option<String> = path_rows.into_iter().next().map(|r| r.path);

        db.query(
            "INSERT INTO links (vault_id, source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number)
             VALUES ($vid, $sp, $tt, $tp, $lt, $rt, $alias, $heading, $ln)
             ON DUPLICATE KEY UPDATE source_path = $sp"
        )
        .bind(("vid", vault_id.clone()))
        .bind(("sp", new_path.clone()))
        .bind(("tt", link.target_title.clone()))
        .bind(("tp", target_path))
        .bind(("lt", link.link_type.clone()))
        .bind(("rt", link.raw_text.clone()))
        .bind(("alias", link.alias.clone()))
        .bind(("heading", link.heading.clone()))
        .bind(("ln", link.line_number))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    }

    db.query("DELETE FROM trash_items WHERE vault_id = $vid AND item_id = $id")
        .bind(("vid", vault_id.clone()))
        .bind(("id", id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;

    Ok(new_path)
}

/// 徹底刪除垃圾桶中的項目（不可復原）
#[tauri::command]
pub async fn delete_trash_items(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<(), AppError> {
    let vault_path = state.get_vault_path().await;
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;
    let trash_dir = PathBuf::from(&vault_path).join(".trash");

    for id in &ids {
        #[derive(Deserialize)]
        struct FilenameRow { trash_filename: String }
        let mut resp = db
            .query("SELECT trash_filename FROM trash_items WHERE vault_id = $vid AND item_id = $id LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("id", id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<FilenameRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

        if let Some(row) = rows.into_iter().next() {
            let file_path = trash_dir.join(&row.trash_filename);
            if file_path.exists() {
                let _ = tokio::fs::remove_file(&file_path).await;
            }
        }
        db.query("DELETE FROM trash_items WHERE vault_id = $vid AND item_id = $id")
            .bind(("vid", vault_id.clone()))
            .bind(("id", id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
    }

    Ok(())
}

async fn scan_dir(
    db: &crate::db::surreal::SurrealDb,
    vault_id: &str,
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
            Box::pin(scan_dir(db, vault_id, vault_root, &path.to_string_lossy(), count)).await?;
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
            #[derive(Deserialize)]
            struct ChecksumRow { checksum: String }
            let mut resp = db
                .query("SELECT checksum FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
                .bind(("vid", vault_id.to_owned()))
                .bind(("path", rel_path.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            let checksum_rows: Vec<ChecksumRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
            let existing_checksum = checksum_rows.into_iter().next().map(|r| r.checksum);

            if existing_checksum.as_deref() == Some(&checksum) {
                continue;
            }

            let now_ms = chrono::Utc::now().timestamp_millis();
            let word_count = crate::vault::count_words(&content);
            let now_dt = surrealdb::sql::Datetime::from(
                chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms).unwrap_or_default()
            );

            db.query(
                "INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at, checksum)
                 VALUES ($vid, $path, $title, $content, $wc, $now, $now, $cs)
                 ON DUPLICATE KEY UPDATE
                   title = $title,
                   content = $content,
                   word_count = $wc,
                   modified_at = $now,
                   checksum = $cs"
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("path", rel_path.clone()))
            .bind(("title", title.clone()))
            .bind(("content", content.clone()))
            .bind(("wc", word_count))
            .bind(("now", now_dt))
            .bind(("cs", checksum.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

            // 更新 graph node
            db.query(
                "INSERT INTO graph_nodes (vault_id, node_id, node_type, label)
                 VALUES ($vid, $path, 'note', $title)
                 ON DUPLICATE KEY UPDATE label = $title"
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("path", rel_path.clone()))
            .bind(("title", title.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;

            // 重新解析 links
            db.query("DELETE FROM links WHERE vault_id = $vid AND source_path = $path")
                .bind(("vid", vault_id.to_owned()))
                .bind(("path", rel_path.clone()))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;

            let parsed_links = crate::vault::indexer::parse_links(&content);
            for link in parsed_links {
                #[derive(Deserialize)]
                struct PathRow { path: String }
                let mut resp = db
                    .query("SELECT path FROM notes WHERE vault_id = $vid AND title = $title LIMIT 1")
                    .bind(("vid", vault_id.to_owned()))
                    .bind(("title", link.target_title.clone()))
                    .await
                    .map_err(|e| AppError::Database(e.to_string()))?;
                let path_rows: Vec<PathRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
                let target_path: Option<String> = path_rows.into_iter().next().map(|r| r.path);

                db.query(
                    "INSERT INTO links (vault_id, source_path, target_title, target_path, link_type, raw_text, alias, heading, line_number)
                     VALUES ($vid, $sp, $tt, $tp, $lt, $rt, $alias, $heading, $ln)
                     ON DUPLICATE KEY UPDATE source_path = $sp"
                )
                .bind(("vid", vault_id.to_owned()))
                .bind(("sp", rel_path.clone()))
                .bind(("tt", link.target_title.clone()))
                .bind(("tp", target_path))
                .bind(("lt", link.link_type.clone()))
                .bind(("rt", link.raw_text.clone()))
                .bind(("alias", link.alias.clone()))
                .bind(("heading", link.heading.clone()))
                .bind(("ln", link.line_number))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            }

            *count += 1;
        }
    }
    Ok(())
}

/// 取得 chunk 索引統計（用於前端顯示進度）
#[tauri::command]
pub async fn get_index_stats(state: State<'_, AppState>) -> Result<serde_json::Value, AppError> {
    use serde_json::json;
    let db = &state.db;
    let vault_id = match state.get_vault_id().await {
        Ok(v) => v,
        Err(_) => return Ok(json!({ "total": 0, "indexed": 0 })),
    };

    #[derive(Deserialize)]
    struct CountRow { count: u64 }

    let mut r1 = db
        .query("SELECT count() AS count FROM notes WHERE vault_id = $vid GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    let total_rows: Vec<CountRow> = r1.take(0).unwrap_or_default();
    let total = total_rows.first().map(|r| r.count).unwrap_or(0);

    // 有至少一個帶 embedding 的 chunk 的筆記數（vector indexed）
    let mut r2 = db
        .query("SELECT count() AS count FROM (SELECT DISTINCT file_path FROM chunks WHERE vault_id = $vid AND embedding != NONE) GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    let emb_rows: Vec<CountRow> = r2.take(0).unwrap_or_default();
    let embedded = emb_rows.first().map(|r| r.count).unwrap_or(0);

    // 有至少一個 chunk 的筆記數（FTS indexed）
    let mut r3 = db
        .query("SELECT count() AS count FROM (SELECT DISTINCT file_path FROM chunks WHERE vault_id = $vid) GROUP ALL")
        .bind(("vid", vault_id.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    let chunk_rows: Vec<CountRow> = r3.take(0).unwrap_or_default();
    let chunked = chunk_rows.first().map(|r| r.count).unwrap_or(0);

    Ok(json!({ "total": total, "chunked": chunked, "embedded": embedded }))
}

/// 重新建立整個 vault 的 chunk 索引，逐筆 emit 進度事件
#[tauri::command]
pub async fn reindex_vault_chunks(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<usize, AppError> {
    use serde::Serialize;
    use tauri::Emitter;

    #[derive(Serialize, Clone)]
    struct ReindexProgress { done: usize, total: usize, path: String }

    #[derive(Deserialize)]
    struct NotePathContent { path: String, content: String }

    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;
    let emb_url = embedding_url(&state).await;

    let mut resp = db
        .query("SELECT path, content FROM notes WHERE vault_id = $vid")
        .bind(("vid", vault_id.clone()))
        .await.map_err(|e| AppError::Database(e.to_string()))?;
    let notes: Vec<NotePathContent> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    let total = notes.len();
    let now = chrono::Utc::now().timestamp_millis();

    for (i, note) in notes.iter().enumerate() {
        let _ = app.emit("reindex:progress", ReindexProgress {
            done: i,
            total,
            path: note.path.clone(),
        });
        let chunks = chunker::chunk_note(&note.path, &note.content, now);
        chunker::upsert_chunks(&db, &vault_id, &chunks, emb_url.as_deref()).await?;
    }

    // 完成
    let _ = app.emit("reindex:progress", ReindexProgress { done: total, total, path: String::new() });
    Ok(total)
}

/// 語意搜尋：chunk FTS + 1-hop graph expansion（供前端 SemanticSearchPanel 使用）
#[tauri::command]
pub async fn search_vault_chunks(
    state: State<'_, AppState>,
    query: String,
) -> Result<String, AppError> {
    use std::collections::{HashMap, HashSet};

    if query.trim().is_empty() {
        return Ok(String::new());
    }
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct ChunkRow {
        file_path: String,
        section: String,
        content: String,
    }

    // 0. 確認 chunks 表有資料（快速診斷）
    {
        #[derive(Deserialize)]
        struct CountRow { count: u64 }
        let mut r = db
            .query("SELECT count() AS count FROM chunks WHERE vault_id = $vid GROUP ALL")
            .bind(("vid", vault_id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<CountRow> = r.take(0).unwrap_or_default();
        let total = rows.first().map(|r| r.count).unwrap_or(0);
        if total == 0 {
            return Ok("🔍 no_index".to_string());
        }
    }

    // 1. 嘗試向量搜尋（若 embedding server 正在運行且 query 可被 embed）
    let emb_url = embedding_url(&state).await;
    let client = reqwest::Client::new();
    // vec_diag: 向量路徑的診斷（優先回報，即使後續 fallback 有結果也記錄）
    let mut vec_diag: Option<&str> = None;
    let mut search_method = "contains";

    let chunk_rows: Vec<ChunkRow> = if let Some(ref url) = emb_url {
        let qvec = crate::commands::ai::get_embedding(&client, url, query.trim()).await;
        if qvec.is_empty() {
            vec_diag = Some("vector_fail");
            vec![]
        } else {
            let mut resp = db
                .query(
                    "SELECT file_path, section, content,
                            vector::similarity::cosine(embedding, $qvec) AS score
                     FROM chunks
                     WHERE vault_id = $vid AND embedding != NONE
                     ORDER BY score DESC
                     LIMIT 10"
                )
                .bind(("vid", vault_id.clone()))
                .bind(("qvec", qvec))
                .await
                .map_err(|e| AppError::Database(e.to_string()))?;
            let rows: Vec<ChunkRow> = resp.take(0).unwrap_or_default();
            if rows.is_empty() {
                vec_diag = Some("no_vec_index");
            } else {
                search_method = "vector";
            }
            rows
        }
    } else {
        vec![]
    };

    // 2. Fallback A：BM25 全文搜尋
    let chunk_rows: Vec<ChunkRow> = if chunk_rows.is_empty() {
        let mut resp = db
            .query(
                "SELECT file_path, section, content FROM chunks
                 WHERE vault_id = $vid AND content @1@ $query
                 LIMIT 10"
            )
            .bind(("vid", vault_id.clone()))
            .bind(("query", query.trim().to_owned()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<ChunkRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
        if !rows.is_empty() { search_method = "bm25"; }
        rows
    } else {
        chunk_rows
    };

    // 3. Fallback B：字串包含搜尋（保底）
    let chunk_rows: Vec<ChunkRow> = if chunk_rows.is_empty() {
        let mut resp = db
            .query(
                "SELECT file_path, section, content FROM chunks
                 WHERE vault_id = $vid
                   AND string::contains(string::lowercase(content), string::lowercase($query))
                 LIMIT 10"
            )
            .bind(("vid", vault_id.clone()))
            .bind(("query", query.trim().to_owned()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let rows: Vec<ChunkRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;
        if !rows.is_empty() { search_method = "contains"; }
        rows
    } else {
        chunk_rows
    };

    if chunk_rows.is_empty() {
        // 若向量路徑有診斷資訊，優先回報（比 fallback 方法名稱更有診斷意義）
        let report = vec_diag.unwrap_or(search_method);
        return Ok(format!("🔍 {}\nno_results", report));
    }

    // 3. Collect matched file paths
    let matched_paths: HashSet<String> = chunk_rows
        .iter()
        .map(|r| r.file_path.clone())
        .collect();

    // 3. Fetch titles
    #[derive(Deserialize)]
    struct TitleRow { path: String, title: String }
    let mut titles: HashMap<String, String> = HashMap::new();
    for path in &matched_paths {
        let mut resp = db
            .query("SELECT path, title FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("path", path.clone()))
            .await
            .unwrap_or_else(|_| unreachable!());
        let rows: Vec<TitleRow> = resp.take(0).unwrap_or_default();
        if let Some(r) = rows.into_iter().next() {
            titles.insert(r.path, r.title);
        }
    }

    // 4. Graph expansion (1-hop)
    #[derive(Deserialize)]
    struct TargetPathRow { target_path: String }
    #[derive(Deserialize)]
    struct SourcePathRow { source_path: String }
    let mut expanded: HashSet<String> = HashSet::new();
    for path in &matched_paths {
        let mut resp = db
            .query("SELECT target_path FROM links WHERE vault_id = $vid AND source_path = $path AND target_path != NONE AND link_type = 'wikilink'")
            .bind(("vid", vault_id.clone()))
            .bind(("path", path.clone()))
            .await
            .unwrap_or_else(|_| unreachable!());
        let out_rows: Vec<TargetPathRow> = resp.take(0).unwrap_or_default();

        let mut resp2 = db
            .query("SELECT source_path FROM links WHERE vault_id = $vid AND target_path = $path AND link_type = 'wikilink'")
            .bind(("vid", vault_id.clone()))
            .bind(("path", path.clone()))
            .await
            .unwrap_or_else(|_| unreachable!());
        let in_rows: Vec<SourcePathRow> = resp2.take(0).unwrap_or_default();

        for r in out_rows {
            if !matched_paths.contains(&r.target_path) {
                expanded.insert(r.target_path);
            }
        }
        for r in in_rows {
            if !matched_paths.contains(&r.source_path) {
                expanded.insert(r.source_path);
            }
        }
    }

    // Fetch expanded titles
    for path in &expanded {
        let mut resp = db
            .query("SELECT path, title FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
            .bind(("vid", vault_id.clone()))
            .bind(("path", path.clone()))
            .await
            .unwrap_or_else(|_| unreachable!());
        let rows: Vec<TitleRow> = resp.take(0).unwrap_or_default();
        if let Some(r) = rows.into_iter().next() {
            titles.insert(r.path, r.title);
        }
    }

    // 5. Build response string
    let mut lines = vec![
        format!("🔍 {}", search_method),
        format!("找到 {} 個相關段落：", chunk_rows.len()),
    ];
    for row in &chunk_rows {
        let title = titles.get(&row.file_path).cloned().unwrap_or_else(|| row.file_path.clone());
        let snippet: String = row.content.chars().take(200).collect();
        let section_label = if row.section.is_empty() { String::new() } else { format!(" § {}", row.section) };
        lines.push(format!("- **{}{}** ({})\n  {}…", title, section_label, row.file_path, snippet.trim()));
    }
    if !expanded.is_empty() {
        lines.push("\n📎 相關連結筆記（透過 wikilink 擴展）：".to_string());
        for path in &expanded {
            let title = titles.get(path).cloned().unwrap_or_else(|| path.clone());
            lines.push(format!("- **{}** ({})", title, path));
        }
    }
    Ok(lines.join("\n"))
}
