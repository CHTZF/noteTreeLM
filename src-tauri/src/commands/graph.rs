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
    let db = state.db.clone();
    let vault_id = state.get_vault_id().await?;

    #[derive(Deserialize)]
    struct NodeRow {
        node_id: String,
        node_type: String,
        label: String,
        url: Option<String>,
        file_path: Option<String>,
    }

    let mut resp = db
        .query(
            "SELECT node_id, node_type, label, url, file_path FROM graph_nodes WHERE vault_id = $vid ORDER BY node_type, label",
        )
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let node_rows: Vec<NodeRow> = resp.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    // 計算每個節點的連結數量
    let mut graph_nodes = Vec::new();
    for row in node_rows {
        #[derive(Deserialize)]
        struct CountRow {
            count: i64,
        }

        let mut resp2 = db
            .query(
                "SELECT count() AS count FROM graph_edges WHERE vault_id = $vid AND (source_id = $nid OR target_id = $nid) GROUP ALL",
            )
            .bind(("vid", vault_id.clone()))
            .bind(("nid", row.node_id.clone()))
            .await
            .map_err(|e| AppError::Database(e.to_string()))?;
        let counts: Vec<CountRow> = resp2.take(0).unwrap_or_default();
        let link_count = counts.first().map(|r| r.count).unwrap_or(0);

        graph_nodes.push(GraphNode {
            id: row.node_id,
            node_type: row.node_type,
            label: row.label,
            url: row.url,
            file_path: row.file_path,
            link_count,
        });
    }

    #[derive(Deserialize)]
    struct EdgeRow {
        source_id: String,
        target_id: String,
        edge_type: String,
        weight: f64,
    }

    let mut resp3 = db
        .query(
            "SELECT source_id, target_id, edge_type, weight FROM graph_edges WHERE vault_id = $vid",
        )
        .bind(("vid", vault_id.clone()))
        .await
        .map_err(|e| AppError::Database(e.to_string()))?;
    let edge_rows: Vec<EdgeRow> = resp3.take(0).map_err(|e| AppError::Database(e.to_string()))?;

    let graph_edges = edge_rows
        .into_iter()
        .map(|r| GraphEdge {
            source_id: r.source_id,
            target_id: r.target_id,
            edge_type: r.edge_type,
            weight: r.weight,
        })
        .collect();

    Ok(GraphData {
        nodes: graph_nodes,
        edges: graph_edges,
    })
}
