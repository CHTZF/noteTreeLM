use crate::{api_client::daemon_get, error::AppError, state::AppState};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub node_type: String,
    pub label: String,
    pub url: Option<String>,
    pub file_path: Option<String>,
    pub link_count: i64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub edge_type: String,
    pub weight: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GraphData {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[tauri::command]
pub async fn get_graph(state: State<'_, AppState>) -> Result<GraphData, AppError> {
    let vault_id = state.get_vault_uuid().await;
    if vault_id.is_empty() {
        return Ok(GraphData { nodes: vec![], edges: vec![] });
    }
    let token = state.get_auth_token().await;
    let tok = if token.is_empty() { None } else { Some(token.as_str()) };
    let path = format!("/vaults/{}/graph", urlencoding::encode(&vault_id));
    let result: serde_json::Value = daemon_get(&state.http_client, &path, tok)
        .await
        .unwrap_or(serde_json::json!({ "nodes": [], "edges": [] }));

    let nodes = result["nodes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|n| {
                    Some(GraphNode {
                        id: n["id"].as_str()?.to_string(),
                        node_type: n["node_type"].as_str().unwrap_or("note").to_string(),
                        label: n["label"].as_str().unwrap_or("").to_string(),
                        url: n["url"].as_str().map(|s| s.to_string()),
                        file_path: n["file_path"].as_str().map(|s| s.to_string()),
                        link_count: n["link_count"].as_i64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let edges = result["edges"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(GraphEdge {
                        source_id: e["source_id"].as_str()?.to_string(),
                        target_id: e["target_id"].as_str()?.to_string(),
                        edge_type: e["edge_type"].as_str().unwrap_or("link").to_string(),
                        weight: e["weight"].as_f64().unwrap_or(1.0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(GraphData { nodes, edges })
}
