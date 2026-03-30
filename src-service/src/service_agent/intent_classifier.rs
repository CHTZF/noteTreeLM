use super::types::EmbedFn;

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Interrupt,
    Cancel,
    Confirm,
    ToolUse,
    Chat,
}

pub struct IntentClassifier;

/// 計算兩個 L2-normalized 向量的 cosine similarity
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// 計算多個 embedding 向量的 centroid（平均向量），並做 L2 正規化
pub fn compute_centroid(vecs: &[Vec<f32>]) -> Vec<f32> {
    if vecs.is_empty() {
        return vec![];
    }
    let dim = vecs[0].len();
    if dim == 0 {
        return vec![];
    }
    let mut centroid = vec![0f32; dim];
    for v in vecs {
        for (i, &f) in v.iter().enumerate() {
            if i < dim { centroid[i] += f; }
        }
    }
    let n = vecs.len() as f32;
    for f in &mut centroid { *f /= n; }
    // L2 normalize
    let norm: f32 = centroid.iter().map(|f| f * f).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for f in &mut centroid { *f /= norm; }
    }
    centroid
}

impl IntentClassifier {

    pub fn new() -> Self { Self }

    /// 確認意圖的標準詞組
    pub fn confirm_phrases() -> &'static [&'static str] {
        &["好", "要", "打開", "開啟", "確認", "可以", "執行", "繼續", "好的", "沒問題"]
    }
    /// 取消意圖的標準詞組
    pub fn cancel_phrases() -> &'static [&'static str] {
        &["不", "算了", "取消", "不要", "停止", "不行", "不用了"]
    }
    /// 中斷意圖的標準詞組
    pub fn interrupt_phrases() -> &'static [&'static str] {
        &["等等", "先停", "暫停", "稍等"]
    }

    pub async fn classify(&self, _input: &str) -> Intent {
        Intent::ToolUse
    }

    /// 計算三組 intent centroid，結果存入快取（Mutex<Option<...>>）。
    /// 已有快取時直接回傳，避免每次 pending_plan 重算 ~21 次 embedding。
    pub async fn compute_centroids_cached(
        cache: &tokio::sync::Mutex<Option<(Vec<f32>, Vec<f32>, Vec<f32>)>>,
        embed_fn: &EmbedFn,
    ) -> Option<(Vec<f32>, Vec<f32>, Vec<f32>)> {
        // 快速路徑：已有快取
        {
            let guard = cache.lock().await;
            if let Some(c) = &*guard {
                return Some(c.clone());
            }
        }

        // 計算三組 centroid
        async fn batch(ef: &EmbedFn, phrases: &[&str]) -> Vec<Vec<f32>> {
            futures_util::future::join_all(
                phrases.iter().map(|p| (ef)(p.to_string()))
            ).await.into_iter().filter(|v| !v.is_empty()).collect()
        }

        let (cv, ccl, ci) = tokio::join!(
            batch(embed_fn, IntentClassifier::confirm_phrases()),
            batch(embed_fn, IntentClassifier::cancel_phrases()),
            batch(embed_fn, IntentClassifier::interrupt_phrases()),
        );
        let cc = compute_centroid(&cv);
        let ccl = compute_centroid(&ccl);
        let ci = compute_centroid(&ci);

        if cc.is_empty() || ccl.is_empty() || ci.is_empty() {
            return None; // embedding server 未就緒，不快取
        }

        let result = (cc, ccl, ci);
        *cache.lock().await = Some(result.clone());
        Some(result)
    }

    /// 用已計算好的 centroid 向量直接分類（無需再呼叫 embedding server）
    pub async fn classify_with_centroids(
        &self,
        input: &str,
        embed_fn: &EmbedFn,
        confirm_c: &[f32],
        cancel_c: &[f32],
        interrupt_c: &[f32],
    ) -> Intent {
        const THRESHOLD: f32 = 0.75;

        let input_vec = embed_fn(input.to_string()).await;
        if input_vec.is_empty() {
            return self.classify(input).await;
        }

        let sim_confirm   = cosine_sim(&input_vec, confirm_c);
        let sim_cancel    = cosine_sim(&input_vec, cancel_c);
        let sim_interrupt = cosine_sim(&input_vec, interrupt_c);

        let max_sim = sim_confirm.max(sim_cancel).max(sim_interrupt);
        if max_sim < THRESHOLD {
            return self.classify(input).await;
        }

        if sim_confirm >= sim_cancel && sim_confirm >= sim_interrupt {
            Intent::Confirm
        } else if sim_cancel >= sim_interrupt {
            Intent::Cancel
        } else {
            Intent::Interrupt
        }
    }

    /// Embedding-based 意圖分類（用於 pending_plan 回應判斷）
    ///
    /// 內部從硬編碼詞組計算 centroid，再與 user input 比較 cosine similarity。
    /// 最高相似度 ≥ 0.75 → 回傳對應 Intent；低於閾值 → fallback 至 keyword rule。
    pub async fn classify_with_embedding(
        &self,
        input: &str,
        embed_fn: &EmbedFn,
    ) -> Intent {
        const THRESHOLD: f32 = 0.75;

        async fn batch(ef: &EmbedFn, phrases: &[&str]) -> Vec<Vec<f32>> {
            futures_util::future::join_all(
                phrases.iter().map(|p| (ef)(p.to_string()))
            ).await.into_iter().filter(|v| !v.is_empty()).collect()
        }

        let (cv, ccl, ci) = tokio::join!(
            batch(embed_fn, IntentClassifier::confirm_phrases()),
            batch(embed_fn, IntentClassifier::cancel_phrases()),
            batch(embed_fn, IntentClassifier::interrupt_phrases()),
        );
        let confirm_centroid   = compute_centroid(&cv);
        let cancel_centroid    = compute_centroid(&ccl);
        let interrupt_centroid = compute_centroid(&ci);

        if confirm_centroid.is_empty() || cancel_centroid.is_empty() || interrupt_centroid.is_empty() {
            return self.classify(input).await;
        }

        let input_vec = embed_fn(input.to_string()).await;
        if input_vec.is_empty() {
            return self.classify(input).await;
        }

        let sim_confirm   = cosine_sim(&input_vec, &confirm_centroid);
        let sim_cancel    = cosine_sim(&input_vec, &cancel_centroid);
        let sim_interrupt = cosine_sim(&input_vec, &interrupt_centroid);

        let max_sim = sim_confirm.max(sim_cancel).max(sim_interrupt);
        if max_sim < THRESHOLD {
            return self.classify(input).await;
        }

        if sim_confirm >= sim_cancel && sim_confirm >= sim_interrupt {
            Intent::Confirm
        } else if sim_cancel >= sim_interrupt {
            Intent::Cancel
        } else {
            Intent::Interrupt
        }
    }
}
