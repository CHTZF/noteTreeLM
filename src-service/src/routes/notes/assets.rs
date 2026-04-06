use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};

use crate::app_state::ApiState;
use crate::routes::vault::get_vault_path;
use super::PathQuery;

pub(super) async fn import_asset_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let filename = body.get("filename").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing filename".to_string()))?;
    let content_b64 = body.get("content_base64").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing content_base64".to_string()))?;
    let folder = body.get("folder").and_then(|v| v.as_str()).unwrap_or("").trim_end_matches('/').to_string();
    let new_name = body.get("new_name").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());

    if filename.contains("..") || filename.contains('/') {
        return Err((StatusCode::BAD_REQUEST, "Invalid filename".to_string()));
    }

    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid base64: {}", e)))?;

    let final_filename = if let Some(name) = new_name {
        let name = name.trim().to_string();
        if std::path::Path::new(&name).extension().is_some() { name }
        else {
            let orig_ext = std::path::Path::new(filename)
                .extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if orig_ext.is_empty() { name } else { format!("{}.{}", name, orig_ext) }
        }
    } else { filename.to_string() };

    let rel_path = if folder.is_empty() { final_filename.clone() }
                   else { format!("{}/{}", folder, final_filename) };

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let dest = std::path::Path::new(&vault_path).join(&rel_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    std::fs::write(&dest, &bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Write failed: {}", e)))?;

    Ok(Json(json!({ "rel_path": rel_path })))
}

pub(super) async fn rename_asset_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = body.get("path").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?.to_string();
    let new_name = body.get("new_name").and_then(|v| v.as_str())
        .ok_or((StatusCode::BAD_REQUEST, "Missing new_name".to_string()))?.trim().to_string();

    if path.contains("..") || new_name.contains("..") || new_name.contains('/') || new_name.contains('\\') {
        return Err((StatusCode::BAD_REQUEST, "Invalid path or new_name".to_string()));
    }

    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(&path);
    let parent = abs_path.parent().ok_or((StatusCode::BAD_REQUEST, "Cannot get parent dir".to_string()))?;
    let new_abs_path = parent.join(&new_name);

    if new_abs_path.exists() {
        return Err((StatusCode::CONFLICT, format!("File {} already exists", new_name)));
    }
    std::fs::rename(&abs_path, &new_abs_path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Rename failed: {}", e)))?;

    let new_rel = new_abs_path.strip_prefix(std::path::Path::new(&vault_path))
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Cannot compute new path".to_string()))?;

    Ok(Json(json!({ "new_path": new_rel })))
}

pub(super) async fn list_assets_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let mut assets: Vec<String> = Vec::new();
    collect_assets_fs(std::path::Path::new(&vault_path), std::path::Path::new(&vault_path), &mut assets);
    Ok(Json(json!(assets)))
}

fn collect_assets_fs(vault_root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') { continue; }
        if path.is_dir() {
            collect_assets_fs(vault_root, &path, out);
        } else {
            let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            if matches!(ext.as_str(), "md" | "markdown" | "mdx") { continue; }
            if let Ok(rel) = path.strip_prefix(vault_root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                if !rel_str.is_empty() { out.push(rel_str); }
            }
        }
    }
}

pub(super) async fn delete_asset_handler(
    State(state): State<ApiState>,
    Path(vault_id): Path<String>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rel_path = q.path.ok_or((StatusCode::BAD_REQUEST, "Missing path".to_string()))?;
    if rel_path.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "Invalid path".to_string()));
    }
    let vault_path = get_vault_path(&state, &vault_id).await?;
    let abs_path = std::path::Path::new(&vault_path).join(&rel_path);
    if abs_path.exists() {
        std::fs::remove_file(&abs_path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}
