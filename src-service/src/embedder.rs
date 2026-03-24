//! Embedding helper — calls the registered embedding server.

/// Call embedding server and return a vector.
/// Returns None if no embedding server is registered or if the call fails.
pub async fn embed_text(
    client: &reqwest::Client,
    embedding_url: &Option<String>,
    text: &str,
) -> Option<Vec<f32>> {
    let url = embedding_url.as_ref()?;
    let resp = client
        .post(format!("{}/embedding", url))
        .json(&serde_json::json!({"input": text}))
        .send()
        .await
        .ok()?;
    let data: serde_json::Value = resp.json().await.ok()?;
    // Handle both {"embedding": [...]} and {"data": [{"embedding": [...]}]}
    let arr = data["embedding"]
        .as_array()
        .or_else(|| data["data"][0]["embedding"].as_array())?;
    Some(arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect())
}
