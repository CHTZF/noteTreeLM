//! Embedding helper — calls the registered embedding server.

/// Cosine similarity (dot product of L2-normalized vectors).
pub fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { return 0.0; }
    dot / (na * nb)
}

/// Call llama-server /embedding with OpenAI + legacy fallback. Returns empty vec on failure.
pub async fn embed_text_llm(client: &reqwest::Client, llm_url: &str, text: &str) -> Vec<f32> {
    fn extract(json: &serde_json::Value) -> Vec<f32> {
        for path in [&json["data"][0]["embedding"], &json["embedding"]] {
            if let Some(arr) = path.as_array() {
                let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
                if !v.is_empty() { return v; }
            }
        }
        if let Some(arr) = json.as_array().and_then(|a| a.first()).and_then(|f| f["embedding"].as_array()) {
            let v: Vec<f32> = arr.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if !v.is_empty() { return v; }
        }
        vec![]
    }
    if let Ok(r) = client.post(format!("{}/v1/embeddings", llm_url))
        .json(&serde_json::json!({"model": "text-embedding", "input": text})).send().await
    {
        if r.status().is_success() {
            if let Ok(j) = r.json::<serde_json::Value>().await {
                let v = extract(&j); if !v.is_empty() { return v; }
            }
        }
    }
    if let Ok(r) = client.post(format!("{}/embedding", llm_url))
        .json(&serde_json::json!({"content": text})).send().await
    {
        if r.status().is_success() {
            if let Ok(j) = r.json::<serde_json::Value>().await {
                let v = extract(&j); if !v.is_empty() { return v; }
            }
        }
    }
    vec![]
}

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
