use super::locale::Locale;

// ── Context pressure & stall (injected transiently each round) ───────────────

pub(crate) fn context_critical_msg(pct: usize, bytes: usize, budget: usize, locale: Locale) -> String {
    match locale {
        Locale::En =>
            format!("[Context] ⚠️ ~{}% used ({} / {} bytes). \
                     Please provide a final response immediately based on information already gathered. \
                     Do not call read_note or other large tools.", pct, bytes, budget),
        _ =>
            format!("[Context] ⚠️ 已使用 ~{}% ({} / {} bytes)。\
                     請立即根據已取得的資訊作出最終回覆，不要再呼叫 read_note 或其他大型工具。",
                    pct, bytes, budget),
    }
}

pub(crate) fn context_warn_msg(pct: usize, bytes: usize, budget: usize, locale: Locale) -> String {
    match locale {
        Locale::En =>
            format!("[Context] ~{}% used ({} / {} bytes). \
                     Consider using search_in_note instead of read_note to locate specific paragraphs \
                     and avoid expanding the context further.", pct, bytes, budget),
        _ =>
            format!("[Context] 已使用 ~{}% ({} / {} bytes)。\
                     建議改用 search_in_note 取代 read_note 以精準定位段落，避免繼續擴大 context。",
                    pct, bytes, budget),
    }
}

pub(crate) fn context_info_msg(pct: usize, bytes: usize, budget: usize, locale: Locale) -> String {
    match locale {
        Locale::En => format!("[Context] ~{}% used ({} / {} bytes)", pct, bytes, budget),
        _          => format!("[Context] 已使用 ~{}% ({} / {} bytes)", pct, bytes, budget),
    }
}

pub(crate) fn stall_warning(tool_names: &[String], locale: Locale) -> String {
    let tools = tool_names.join(if matches!(locale, Locale::En) { ", " } else { "、" });
    match locale {
        Locale::En =>
            format!("[System] You have repeatedly called: {}. \
                     Please try a different approach, call get_session_state to review execution history, \
                     or respond based on information already gathered.", tools),
        _ =>
            format!("[系統提示] 你在本 session 已重複呼叫：{}。\
                     請換策略，或呼叫 get_session_state 查看完整執行歷史再決定下一步，\
                     或直接根據已取得的資訊回覆使用者。", tools),
    }
}

// ── Compress & truncation ────────────────────────────────────────────────────

pub(crate) fn compress_summary_msg(summary: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!("[Summary] Key information from previous tool executions:\n{}", summary),
        _          => format!("[壓縮摘要] 以下是前幾輪工具執行的關鍵資訊：\n{}", summary),
    }
}

pub(crate) fn auto_compress_summary(count: usize, names: &[String], locale: Locale) -> String {
    let joined = names.join(if matches!(locale, Locale::En) { ", " } else { "、" });
    match locale {
        Locale::En => format!("[Auto-compress] Executed {} tool operations: {}", count, joined),
        _          => format!("[自動壓縮] 已執行 {} 個工具操作：{}", count, joined),
    }
}

pub(crate) fn tool_result_truncated(cut: usize, total: usize, content: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!(
            "(First {cut} chars / {total} total)\n{content}\n\
             (Truncated, {} chars not shown. \
             Use read_note with offset/limit for pagination, or search_in_note for precise location)",
            total - cut
        ),
        _ => format!(
            "（前 {cut} 字 / 共 {total} 字）\n{content}\n\
             （截斷，剩餘 {} 字未顯示。使用 read_note 的 offset/limit 分頁讀取，或 search_in_note 精準定位）",
            total - cut
        ),
    }
}

// ── History summarisation ────────────────────────────────────────────────────

pub(crate) fn history_summarise_system(locale: Locale) -> &'static str {
    match locale {
        Locale::En =>
            "You are a conversation summarizer. Compress the following conversation history into \
             2-5 key bullet points, preserving key requirements, decisions, and context. \
             Reply without any prefix.",
        _ =>
            "你是對話摘要助手。請將以下對話歷史壓縮為 2-5 句重點摘要，\
             保留關鍵需求、決策和上下文。用繁體中文回答，不要加任何前綴。",
    }
}

pub(crate) fn history_summary_msg(summary: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!("[Conversation Summary]\n{}", summary),
        _          => format!("[對話摘要]\n{}", summary),
    }
}

// ── Pipeline / system message assembly ──────────────────────────────────────

pub(crate) fn citation_protocol(locale: Locale) -> &'static str {
    match locale {
        Locale::En =>
            "Tool results contain __cite_id__ field; the first sentence of your final reply \
             must cite the tool results used in [cite:id1,id2] format, \
             or output [cite:none] if no tools were used this round. \
             NEVER fabricate citation IDs — only use IDs that appear in actual tool results. \
             Do NOT place [cite:...] anywhere except the very first sentence.",
        _ =>
            "工具結果中含有 __cite_id__ 欄位；最終文字回覆的第一句必須以 \
             [cite:id1,id2] 格式引用所依據的工具結果，若本輪未使用任何工具則輸出 [cite:none]。\
             絕對禁止捏造 cite_id——只能引用實際工具結果中出現的 __cite_id__ 值。\
             [cite:...] 只能出現在回覆的第一句，禁止在其他位置插入。",
    }
}

pub(crate) fn activity_context_block(ac: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!("[User Activity Log]\n{}", ac),
        _          => format!("[使用者活動紀錄]\n{}", ac),
    }
}

pub(crate) fn memory_block(lines: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!("## Related Memories\n{}", lines),
        _          => format!("## 相關記憶\n{}", lines),
    }
}

// ── System message injectors ─────────────────────────────────────────────────

pub(crate) fn checkpoint_injection(done: &str, remaining: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!(
            "## Task Checkpoint (Incomplete from last session)\n\
             ### Completed\n{}\n\n\
             ### Remaining\n{}\n\n\
             Continue from the above progress, call clear_checkpoint when done.",
            done, remaining
        ),
        _ => format!(
            "## 任務 Checkpoint（上次未完成）\n\
             ### 已完成\n{}\n\n\
             ### 尚待處理\n{}\n\n\
             請根據以上進度繼續執行，完成後呼叫 clear_checkpoint。",
            done, remaining
        ),
    }
}

pub(crate) fn agent_knowledge_injection(entries: &str, locale: Locale) -> String {
    match locale {
        Locale::En => format!("## Operational Knowledge for This Vault\n{}", entries),
        _          => format!("## 關於此 Vault 的操作知識\n{}", entries),
    }
}
