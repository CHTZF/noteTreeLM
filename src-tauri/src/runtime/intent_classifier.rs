use serde::Deserialize;

use crate::db::surreal::SurrealDb;
use crate::runtime::types::EmbedFn;

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Interrupt,
    Cancel,
    Confirm,
    /// 記憶查詢意圖：路由至 MemoryAgent（非串流 LLM loop）
    Memory,
    ToolUse,
    Chat,
}

pub struct IntentClassifier {
    db: SurrealDb,
}

#[derive(Deserialize)]
struct KwRow {
    intent: String,
    keywords: String,
}

/// 計算兩個 L2-normalized 向量的 cosine similarity
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

impl IntentClassifier {

    pub fn new(db: SurrealDb) -> Self {
        Self { db }
    }

    pub async fn classify(&self, input: &str) -> Intent {

        let input_trimmed = input.trim();
        let input_lower = input_trimmed.to_lowercase();

        // 查詢所有 intent_keywords 列
        let mut resp = match self.db.query("SELECT intent, keywords FROM intent_keywords").await.ok() {
            Some(r) => r,
            None => return Intent::ToolUse,
        };
        let rows: Vec<KwRow> = resp.take(0).unwrap_or_default();

        for row in rows {
            let intent_str = row.intent;
            let keywords_json = row.keywords;

            let keywords: Vec<String> = match serde_json::from_str(&keywords_json) {
                Ok(v) => v,
                Err(_) => continue,
            };

            for kw in &keywords {
                let kw_lower = kw.to_lowercase();
                let kw_chars = kw.chars().count();

                // 短關鍵字（≤5字）用精確比對，長的用子字串比對
                let matched = if kw_chars <= 5 {
                    input_trimmed == kw.as_str() || input_lower == kw_lower
                } else {
                    input_lower.contains(&kw_lower)
                };

                if matched {
                    return match intent_str.as_str() {
                        "CANCEL"    => Intent::Cancel,
                        "INTERRUPT" => Intent::Interrupt,
                        "CONFIRM"   => Intent::Confirm,
                        "MEMORY"    => Intent::Memory,
                        _           => Intent::Chat,
                    };
                }
            }
        }

        // 預設：當作工具呼叫任務
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
