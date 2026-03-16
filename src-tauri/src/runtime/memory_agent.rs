// memory_agent.rs
//
// MemoryAgent：提供記憶查詢的系統 Prompt 與工具定義（無狀態靜態方法）。
//
// 公開入口：
//   MemoryAgent::build_system_prompt() — 動態建立記憶查詢系統提示詞
//   MemoryAgent::tools_definition()    — query_memory + add_memory_rule 工具定義
//
// 輔助函式：
//   parse_query_since*                 — 時間表達式解析（純 Rust，< 1µs）
//   tool_query_memory                  — 查詢記憶 DB，格式化純文字
//   parse_text_tool_calls              — 解析 <tool_call>...</tool_call> 格式

use chrono::{Datelike, Local};
use serde::Deserialize;
use serde_json::Value;

use crate::db::surreal::SurrealDb;

// ── MemoryAgent ───────────────────────────────────────────────────────────────

/// 記憶查詢的系統 Prompt 與工具定義提供者（無狀態，僅提供靜態方法）
pub struct MemoryAgent;

impl MemoryAgent {
    /// 記憶查詢 System Prompt（含今日/昨日等動態日期）
    pub fn build_system_prompt() -> String {
        let now = Local::now();
        let today_str      = now.format("%Y-%m-%d").to_string();
        let yesterday_str  = (now - chrono::TimeDelta::days(1)).format("%Y-%m-%d").to_string();
        let day_before_str = (now - chrono::TimeDelta::days(2)).format("%Y-%m-%d").to_string();
        let this_month_str = now.format("%Y-%m-01").to_string();
        let last_month_str = {
            let (y, m) = (now.year(), now.month());
            if m == 1 { format!("{}-12-01", y - 1) } else { format!("{}-{:02}-01", y, m - 1) }
        };
        format!(
            "你是一個記憶查詢助手。今天是 {today}。\
根據使用者的問題，搜尋相關的過去對話記憶，整理成簡潔摘要後直接輸出。\n\
【時間表達式轉換規則】\n\
- 剛剛／剛才／最近 → keywords=[], 不帶 since\n\
- 今天 → since=\"{today}\"  昨天 → since=\"{yesterday}\"  前天 → since=\"{day_before}\"\n\
- N天前（N≥3）→ since 自行計算  本月 → since=\"{this_month}\"  上月 → since=\"{last_month}\"\n\
- 本週 → since 為本週一（今天是週{weekday}）  X月 → since=\"{{year}}-{{X:02}}-01\"\n\
- 遇到 Rust 不認識的時間表達式（如「3小時前」「大前天」「上上週」）→ 先呼叫 add_memory_rule 儲存規則，再呼叫 query_memory\n\
【add_memory_rule 規則】\n\
  temporal_exact_days: 固定天數，value 為負整數（如「大前天」\"-3\"）\n\
  temporal_unit: 數字+後綴，value 為 hours/minutes/weeks（如「小時前」\"hours\"）\n\
  stopword: 應過濾的停用字，value 為空字串\n\
【輸出規則】只輸出記憶摘要，不對話，不提問。找不到記憶只回覆「未找到相關記憶」。",
            today = today_str, yesterday = yesterday_str, day_before = day_before_str,
            this_month = this_month_str, last_month = last_month_str,
            weekday = now.weekday().number_from_monday(),
        )
    }

    /// 固定的兩個工具定義（query_memory + add_memory_rule）
    pub fn tools_definition() -> Value {
        serde_json::json!([
            {
                "type": "function",
                "function": {
                    "name": "query_memory",
                    "description": "搜尋過去對話記憶。keywords 空陣列=取最新記憶；有關鍵字=FTS 搜尋。since 為時間下限 YYYY-MM-DD。",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "keywords": { "type": "array", "items": { "type": "string" },
                                "description": "搜尋關鍵字，空陣列=最新記憶" },
                            "since":    { "type": "string",  "description": "時間下限 YYYY-MM-DD" },
                            "limit":    { "type": "integer", "description": "最多筆數，預設 3" }
                        },
                        "required": []
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "add_memory_rule",
                    "description": "發現 Rust 不認識的時間表達式時，儲存規則讓系統下次直接處理（不再需要 LLM）",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern_type": { "type": "string",
                                "enum": ["temporal_exact_days","temporal_unit","stopword"],
                                "description": "規則類型" },
                            "pattern": { "type": "string",
                                "description": "觸發字串，如「大前天」「小時前」" },
                            "value": { "type": "string",
                                "description": "temporal_exact_days: 負整數如\"-3\"；temporal_unit: hours/minutes/weeks；stopword: 空字串" }
                        },
                        "required": ["pattern_type","pattern","value"]
                    }
                }
            }
        ])
    }
}

// ── 字元工具 ─────────────────────────────────────────────────────────────────

/// 判斷是否為 CJK 漢字
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF |
        0x20000..=0x2A6DF | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F
    )
}

// ── 時間解析 ─────────────────────────────────────────────────────────────────

/// 從查詢字串解析時間表達式，回傳 since 毫秒時間戳（None = 不限時間）
pub(crate) fn parse_query_since(query: &str, now: &chrono::DateTime<Local>) -> Option<i64> {
    let day_start = |d: chrono::NaiveDate| -> Option<i64> {
        d.and_hms_opt(0, 0, 0)?
            .and_local_timezone(Local).earliest()
            .map(|dt| dt.timestamp_millis())
    };

    if query.contains("今天") {
        return day_start(now.date_naive());
    }
    if query.contains("昨天") {
        return day_start(now.date_naive() - chrono::TimeDelta::days(1));
    }
    if query.contains("前天") {
        return day_start(now.date_naive() - chrono::TimeDelta::days(2));
    }

    // "3天前" / "三天前"
    let chars: Vec<char> = query.chars().collect();
    for w in chars.windows(3) {
        if w[1] == '天' && w[2] == '前' {
            let days: Option<i64> = if let Some(d) = w[0].to_digit(10) {
                Some(d as i64)
            } else {
                [('一',1i64),('二',2),('三',3),('四',4),('五',5),
                 ('六',6),('七',7),('八',8),('九',9)]
                    .iter().find(|&&(c, _)| c == w[0]).map(|&(_, v)| v)
            };
            if let Some(d) = days {
                return day_start(now.date_naive() - chrono::TimeDelta::days(d));
            }
        }
    }

    if query.contains("本週") || query.contains("這週") || query.contains("本周") || query.contains("這周") {
        let offset = now.weekday().num_days_from_monday() as i64;
        return day_start(now.date_naive() - chrono::TimeDelta::days(offset));
    }
    if query.contains("本月") || query.contains("這個月") || query.contains("這月") {
        return day_start(chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)?);
    }
    if query.contains("上個月") || query.contains("上月") {
        let (y, m) = if now.month() == 1 { (now.year() - 1, 12u32) } else { (now.year(), now.month() - 1) };
        return day_start(chrono::NaiveDate::from_ymd_opt(y, m, 1)?);
    }

    // 中文月份 "一月".."十二月" / 阿拉伯 "1月".."12月"
    let cn_months = [("一月",1u32),("二月",2),("三月",3),("四月",4),
                     ("五月",5),("六月",6),("七月",7),("八月",8),
                     ("九月",9),("十月",10),("十一月",11),("十二月",12)];
    for &(name, m) in &cn_months {
        if query.contains(name) {
            return day_start(chrono::NaiveDate::from_ymd_opt(now.year(), m, 1)?);
        }
    }
    for m in 1u32..=12 {
        if query.contains(&format!("{}月", m)) {
            return day_start(chrono::NaiveDate::from_ymd_opt(now.year(), m, 1)?);
        }
    }

    None // 剛剛/剛才/最近/不限時間
}

/// 從查詢字串中提取模式前方的數字（阿拉伯或中文，供 temporal_unit 規則使用）
fn extract_number_before(query: &str, suffix: &str) -> Option<i64> {
    let pos = query.find(suffix)?;
    let before: Vec<char> = query[..pos].chars().collect();
    // 阿拉伯數字（從尾端往前取連續數字）
    let digits: String = before.iter().rev().take_while(|c| c.is_ascii_digit())
        .collect::<String>().chars().rev().collect();
    if !digits.is_empty() { return digits.parse().ok(); }
    // 中文數字（單字）
    let last = before.last()?;
    [('一',1i64),('二',2),('三',3),('四',4),('五',5),
     ('六',6),('七',7),('八',8),('九',9),('十',10)]
        .iter().find(|&&(c,_)| c == *last).map(|&(_,v)| v)
}

/// 載入 memory_rules 表中的規則
pub(crate) async fn load_memory_rules(db: &SurrealDb, vault_id: &str) -> Vec<(String, String, String)> {
    #[derive(Deserialize)]
    struct Row { pattern_type: String, pattern: String, value: String }
    let vid = vault_id.to_owned();
    let mut resp = match db.query("SELECT pattern_type, pattern, value FROM memory_rules WHERE vault_id = $vid")
        .bind(("vid", vid))
        .await { Ok(r) => r, Err(_) => return vec![] };
    let rows: Vec<Row> = resp.take(0).unwrap_or_default();
    rows.into_iter().map(|r| (r.pattern_type, r.pattern, r.value)).collect()
}

/// 與 parse_query_since 相同，但先套用資料庫中的自訂規則
pub(crate) fn parse_query_since_with_rules(
    query: &str,
    now: &chrono::DateTime<Local>,
    rules: &[(String, String, String)],
) -> Option<i64> {
    let day_start = |d: chrono::NaiveDate| -> Option<i64> {
        d.and_hms_opt(0, 0, 0)?.and_local_timezone(Local).earliest()
            .map(|dt| dt.timestamp_millis())
    };
    // 先套用自訂規則
    for (ptype, pattern, value) in rules {
        match ptype.as_str() {
            "temporal_exact_days" if query.contains(pattern.as_str()) => {
                let days: i64 = value.parse().unwrap_or(0);
                return day_start(now.date_naive() + chrono::TimeDelta::days(days));
            }
            "temporal_unit" if query.contains(pattern.as_str()) => {
                if let Some(n) = extract_number_before(query, pattern) {
                    return Some(match value.as_str() {
                        "hours"   => (now.clone() - chrono::TimeDelta::hours(n)).timestamp_millis(),
                        "minutes" => (now.clone() - chrono::TimeDelta::minutes(n)).timestamp_millis(),
                        "weeks"   => return day_start(now.date_naive() - chrono::TimeDelta::weeks(n)),
                        _         => return None,
                    });
                }
            }
            _ => {}
        }
    }
    // 回退到內建規則
    parse_query_since(query, now)
}

/// 與 extract_cjk_bigrams 相同，但合併來自 memory_rules 的額外停用詞
pub(crate) fn extract_cjk_bigrams_with_extra_stops(query: &str, extra_stops: &[char]) -> Vec<String> {
    const STOPS: &[char] = &[
        '你','我','他','她','它','的','了','嗎','啊','哦','嗯','是','有','在',
        '說','知','道','記','得','什','麼','怎','樣','那','這','就','都','也',
        '還','不','沒','要','會','可','以','和','與','或','但','如','果','因',
        '為','所','而','且','雖','然','呢','嘛','吧','喔','囉','啦','呀','嘿',
        '哈','去','來','到','對','把','被','讓','幫','請','謝','好','很',
    ];
    let cjk: Vec<char> = query.chars().filter(|c| is_cjk(*c)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for pair in cjk.windows(2) {
        let in_stops = |c: char| STOPS.contains(&c) || extra_stops.contains(&c);
        if in_stops(pair[0]) && in_stops(pair[1]) { continue; }
        let bigram: String = pair.iter().collect();
        if seen.insert(bigram.clone()) { out.push(bigram); }
    }
    out
}

// ── 記憶 DB 工具 ──────────────────────────────────────────────────────────────

/// 將規則寫入 memory_rules 表（供 add_memory_rule command 共用）
pub(crate) async fn add_memory_rule_to_db(db: &SurrealDb, vault_id: &str, pattern_type: &str, pattern: &str, value: &str) -> String {
    let vid = vault_id.to_owned();
    let pt = pattern_type.to_owned();
    let p = pattern.to_owned();
    let v = value.to_owned();
    let result_msg = format!("已記住規則：{} = {}", pattern, value);
    match db.query("INSERT INTO memory_rules (vault_id, pattern_type, pattern, value) VALUES ($vid, $pt, $p, $v) ON DUPLICATE KEY UPDATE value = $v, pattern_type = $pt")
        .bind(("vid", vid))
        .bind(("pt", pt))
        .bind(("p", p))
        .bind(("v", v))
        .await {
        Ok(_) => result_msg,
        Err(e) => format!("儲存規則失敗：{}", e),
    }
}

/// 格式化記憶筆記列表為純文字（供 LLM context 使用）
pub(crate) async fn format_memory_rows(rows: &[(String, String, i64)], prefix: &str, vault_db: &SurrealDb, vault_id: &str) -> String {
    let mut output = if prefix.is_empty() {
        format!("找到 {} 筆記憶筆記：\n\n", rows.len())
    } else {
        format!("{}\n找到 {} 筆記憶筆記：\n\n", prefix, rows.len())
    };

    #[derive(Deserialize)]
    struct ContentRow { content: String }

    let vid = vault_id.to_owned();

    for (path, title, created_ms) in rows {
        let dt = chrono::DateTime::from_timestamp_millis(*created_ms)
            .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "未知時間".to_string());

        // 取前 600 字元的摘要（跳過 frontmatter 分隔符）
        let path_owned = path.clone();
        let vid_owned = vid.clone();
        let mut resp = vault_db.query("SELECT content FROM notes WHERE vault_id = $vid AND path = $path LIMIT 1")
            .bind(("vid", vid_owned))
            .bind(("path", path_owned))
            .await.ok();
        let content = resp.as_mut()
            .and_then(|r| r.take::<Vec<ContentRow>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.content)
            .unwrap_or_default();

        let snippet: String = content
            .chars()
            .skip_while(|c| *c == '-' || *c == '\n')
            .take(600)
            .collect();

        output.push_str(&format!("【{}】{}\n路徑：{}\n內容：\n{}…\n\n", dt, title, path, snippet.trim()));
    }
    output
}

/// Agent 工具：查詢記憶筆記（回傳格式化純文字，供 LLM 直接使用）
pub(crate) async fn tool_query_memory(
    keywords: Vec<String>,
    since: Option<String>,
    limit: Option<usize>,
    vault_db: &SurrealDb,
    vault_id: &str,
) -> String {
    let limit = limit.unwrap_or(3).min(10) as i64;

    // 可選：時間篩選（since 格式 YYYY-MM-DD）
    let since_ts: Option<i64> = since.and_then(|s| {
        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok().map(|d| {
            d.and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_local_timezone(Local).earliest())
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0)
        })
    });

    #[derive(Deserialize)]
    struct NoteRow { path: String, title: String, modified_at: i64 }

    let rows: Vec<(String, String, i64)> = if keywords.is_empty() {
        // 無關鍵字：按時間降序回傳最新記憶
        let mut resp = vault_db.query(
            "SELECT path, title, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, 'memories/') ORDER BY modified_at DESC LIMIT $lim"
        )
        .bind(("vid", vault_id.to_owned()))
        .bind(("lim", limit))
        .await;

        let mut rows: Vec<(String, String, i64)> = resp
            .as_mut()
            .ok()
            .and_then(|r| r.take::<Vec<NoteRow>>(0).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.path, r.title, r.modified_at))
            .collect();

        if let Some(min_ts) = since_ts {
            rows.retain(|(_, _, ts)| *ts >= min_ts);
        }
        rows
    } else {
        // 有關鍵字：對每個關鍵字使用 FTS 搜尋，合併去重
        let mut seen = std::collections::HashSet::new();
        let mut collected: Vec<(String, String, i64)> = Vec::new();

        for kw in &keywords {
            let mut resp = vault_db.query(
                "SELECT path, title, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, 'memories/') AND content ~ $kw ORDER BY modified_at DESC LIMIT $lim"
            )
            .bind(("vid", vault_id.to_owned()))
            .bind(("kw", kw.to_owned()))
            .bind(("lim", limit))
            .await;

            let kw_rows: Vec<(String, String, i64)> = resp
                .as_mut()
                .ok()
                .and_then(|r| r.take::<Vec<NoteRow>>(0).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.path, r.title, r.modified_at))
                .collect();

            for row in kw_rows {
                if seen.insert(row.0.clone()) {
                    collected.push(row);
                }
            }
        }

        collected.into_iter()
            .filter(|(_, _, ts)| since_ts.map_or(true, |min_ts| *ts >= min_ts))
            .take(limit as usize)
            .collect()
    };

    if rows.is_empty() {
        if keywords.is_empty() {
            return "目前沒有任何已儲存的記憶筆記".to_string();
        }
        // FTS 找不到時，降級到最新 1 筆，避免空手而回
        let mut resp = vault_db.query(
            "SELECT path, title, modified_at FROM notes WHERE vault_id = $vid AND string::starts_with(path, 'memories/') ORDER BY modified_at DESC LIMIT 1"
        )
        .bind(("vid", vault_id.to_owned()))
        .await;

        let fallback: Vec<(String, String, i64)> = resp
            .as_mut()
            .ok()
            .and_then(|r| r.take::<Vec<NoteRow>>(0).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.path, r.title, r.modified_at))
            .collect();

        if fallback.is_empty() {
            return format!("未找到關鍵字「{}」相關的記憶筆記，且目前無任何記憶存檔", keywords.join("、"));
        }
        return format_memory_rows(&fallback, &format!("（關鍵字「{}」無精確匹配，以下為最新記憶）", keywords.join("、")), vault_db, vault_id).await;
    }

    format_memory_rows(&rows, "", vault_db, vault_id).await
}

/// 解析 LLM 以文字格式輸出的工具調用
/// 支援格式：<tool_call>{"name":"func","arguments":{...}}</tool_call>
pub(crate) fn parse_text_tool_calls(content: &str) -> Vec<Value> {
    let mut calls = Vec::new();
    let mut remaining = content;
    while let Some(start) = remaining.find("<tool_call>") {
        let after_open = &remaining[start + "<tool_call>".len()..];
        if let Some(end) = after_open.find("</tool_call>") {
            let json_str = after_open[..end].trim();
            if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                let name = v["name"].as_str().unwrap_or("").to_string();
                let args_str = serde_json::to_string(&v["arguments"])
                    .unwrap_or_else(|_| "{}".to_string());
                calls.push(serde_json::json!({
                    "id": format!("call_{}", name),
                    "type": "function",
                    "function": { "name": name, "arguments": args_str }
                }));
            }
            remaining = &after_open[end + "</tool_call>".len()..];
        } else {
            break;
        }
    }
    calls
}
