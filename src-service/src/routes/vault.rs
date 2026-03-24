use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::path::Path as FsPath;
use uuid::Uuid;

use crate::api_state::ApiState;

pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/vaults", get(list_vaults).post(register_vault))
        .route("/vaults/:vault_id/structure", get(vault_structure))
        .route("/vaults/:vault_id/scan", post(scan_vault))
}

async fn list_vaults(
    State(state): State<ApiState>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut resp = state
        .db
        .query("SELECT * FROM vaults ORDER BY created_at DESC")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Value> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!(rows)))
}

async fn register_vault(
    State(state): State<ApiState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?
        .to_string();
    let account = body
        .get("account")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let vault_id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();

    state
        .db
        .query("INSERT INTO vaults (vault_id, path, account, created_at) VALUES ($vid, $path, $account, $now)")
        .bind(("vid", vault_id.clone()))
        .bind(("path", path))
        .bind(("account", account))
        .bind(("now", now))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({ "vault_id": vault_id })))
}

async fn vault_structure(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let tree = build_tree(FsPath::new(&vault_path), FsPath::new(&vault_path));
    Ok(Json(json!({ "vault_id": vault_id, "path": vault_path, "tree": tree })))
}

async fn scan_vault(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let root = FsPath::new(&vault_path);

    let md_files = collect_md_files(root);
    let now = Utc::now().timestamp();
    let mut indexed = 0usize;

    for file_path in md_files {
        let rel_path = file_path
            .strip_prefix(root)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let title = extract_title(&content, &rel_path);
        let word_count = content.split_whitespace().count() as i64;
        let modified_at = file_path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(now);

        let _ = state
            .db
            .query("INSERT INTO notes (vault_id, path, title, content, word_count, created_at, modified_at) VALUES ($vid, $path, $title, $content, $wc, $now, $mod) ON DUPLICATE KEY UPDATE title = $title, content = $content, word_count = $wc, modified_at = $mod")
            .bind(("vid", vault_id.clone()))
            .bind(("path", rel_path))
            .bind(("title", title))
            .bind(("content", content))
            .bind(("wc", word_count))
            .bind(("now", now))
            .bind(("mod", modified_at))
            .await;

        indexed += 1;
    }

    Ok(Json(json!({ "ok": true, "indexed": indexed })))
}

pub async fn get_vault_path(
    state: &ApiState,
    vault_id: &str,
) -> Result<String, (StatusCode, String)> {
    #[derive(serde::Deserialize)]
    struct Row {
        path: String,
    }

    let vault_id_owned = vault_id.to_string();
    let mut resp = state
        .db
        .query("SELECT path FROM vaults WHERE vault_id = $vid LIMIT 1")
        .bind(("vid", vault_id_owned))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<Row> = resp
        .take(0)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    rows.into_iter()
        .next()
        .map(|r| r.path)
        .ok_or((StatusCode::NOT_FOUND, format!("Vault '{}' not found", vault_id)))
}

fn build_tree(path: &FsPath, root: &FsPath) -> Value {
    if path.is_dir() {
        let mut children = Vec::new();
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| {
                let is_dir = e.path().is_dir();
                let name = e.file_name().to_string_lossy().to_lowercase();
                (!is_dir, name)
            });
            for entry in entries {
                let p = entry.path();
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                // Skip hidden files/dirs
                if name.starts_with('.') {
                    continue;
                }
                children.push(build_tree(&p, root));
            }
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| rel.clone());
        json!({
            "type": "folder",
            "name": name,
            "path": rel,
            "children": children,
        })
    } else {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| rel.clone());
        json!({
            "type": "file",
            "name": name,
            "path": rel,
        })
    }
}

fn collect_md_files(root: &FsPath) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    collect_md_recursive(root, &mut files);
    files
}

fn collect_md_recursive(dir: &FsPath, files: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_md_recursive(&path, files);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            files.push(path);
        }
    }
}

fn extract_title(content: &str, fallback_path: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("# ") {
            return heading.trim().to_string();
        }
        if !trimmed.is_empty() {
            break;
        }
    }
    // Fallback: filename without extension
    FsPath::new(fallback_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback_path.to_string())
}
