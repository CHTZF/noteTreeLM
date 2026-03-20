use crate::runtime::types::EmbedFn;

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

    /// Embedding-based 意圖分類（用於 pending_plan 回應判斷）
    ///
    /// 內部從硬編碼詞組計算 centroid，再與 user input 比較 cosine similarity。
    /// 最高相似度 ≥ 0.75 → 回傳對應 Intent；低於閾值 → fallback 至 keyword rule。
    pub async fn classify_with_embedding(
        &self,
        input: &str,
        embed_fn: &EmbedFn,
    ) -> Intent {
        use crate::commands::ai::compute_centroid;
        const THRESHOLD: f32 = 0.75;

        async fn batch(ef: &EmbedFn, phrases: &[&str]) -> Vec<Vec<f32>> {
            futures::future::join_all(
                phrases.iter().map(|p| (ef)(p.to_string()))
            ).await.into_iter().filter(|v| !v.is_empty()).collect()
        }

        let (cv, ccl, ci) = tokio::join!(
            batch(embed_fn, Self::confirm_phrases()),
            batch(embed_fn, Self::cancel_phrases()),
            batch(embed_fn, Self::interrupt_phrases()),
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
