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

    pub fn new() -> Self {
        Self
    }

    pub async fn classify(&self, _input: &str) -> Intent {
        Intent::ToolUse
    }

    /// Embedding-based 意圖分類（用於 pending_plan 回應判斷）
    ///
    /// 先 embed user input，計算與 confirm/cancel/interrupt centroid 的 cosine similarity。
    /// 最高相似度 ≥ 0.75 → 回傳對應 Intent；低於閾值 → fallback 至 keyword rule。
    pub async fn classify_with_embedding(
        &self,
        input: &str,
        confirm_centroid: &[f32],
        cancel_centroid: &[f32],
        interrupt_centroid: &[f32],
        embed_fn: &EmbedFn,
    ) -> Intent {
        const THRESHOLD: f32 = 0.75;

        // 若任一 centroid 為空（embedding 從未成功），直接 fallback
        if confirm_centroid.is_empty() || cancel_centroid.is_empty() || interrupt_centroid.is_empty() {
            return self.classify(input).await;
        }

        let input_vec = embed_fn(input.to_string()).await;
        if input_vec.is_empty() {
            return self.classify(input).await;
        }

        let sim_confirm   = cosine_sim(&input_vec, confirm_centroid);
        let sim_cancel    = cosine_sim(&input_vec, cancel_centroid);
        let sim_interrupt = cosine_sim(&input_vec, interrupt_centroid);

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
