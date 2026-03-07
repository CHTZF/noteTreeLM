use sqlx::SqlitePool;
use sqlx::Row;

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
    db: SqlitePool,
}

impl IntentClassifier {

    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    pub async fn classify(&self, input: &str) -> Intent {

        let input_trimmed = input.trim();
        let input_lower = input_trimmed.to_lowercase();

        // 查詢所有 intent_keywords 列
        let rows = sqlx::query("SELECT intent, keywords FROM intent_keywords")
            .fetch_all(&self.db)
            .await;

        if let Ok(rows) = rows {
            for row in rows {
                let intent_str: String = row.get("intent");
                let keywords_json: String = row.get("keywords");

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
        }

        // 預設：當作工具呼叫任務
        Intent::ToolUse
    }
}
