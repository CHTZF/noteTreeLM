use crate::{error::AppError, state::AppState};
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
    pub id: i64,
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
    let nodes = sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
        "SELECT n.id, n.node_type, n.label, n.url, n.file_path
         FROM graph_nodes n
         WHERE n.node_type != 'note'
            OR EXISTS (SELECT 1 FROM notes WHERE path = n.id)
         ORDER BY n.node_type, n.label"
    )
    .fetch_all(&state.db)
    .await?;

    // 計算每個節點的連結數量
    let mut graph_nodes = Vec::new();
    for (id, node_type, label, url, file_path) in nodes {
        let link_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM graph_edges WHERE source_id = ? OR target_id = ?"
        )
        .bind(&id)
        .bind(&id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

        graph_nodes.push(GraphNode {
            id,
            node_type,
            label,
            url,
            file_path,
            link_count,
        });
    }

    let edges = sqlx::query_as::<_, (i64, String, String, String, f64)>(
        "SELECT id, source_id, target_id, edge_type, weight FROM graph_edges"
    )
    .fetch_all(&state.db)
    .await?;

    let graph_edges = edges
        .into_iter()
        .map(|(id, source_id, target_id, edge_type, weight)| GraphEdge {
            id,
            source_id,
            target_id,
            edge_type,
            weight,
        })
        .collect();

    Ok(GraphData {
        nodes: graph_nodes,
        edges: graph_edges,
    })
}
