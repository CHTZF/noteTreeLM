/// commands/fts_helpers.rs
///
/// FTS 查詢清洗與數值比較過濾：
///   clean_fts_query / Comparison / parse_comparison / filter_lines_by_comparison

// ── FTS 查詢清洗 ──────────────────────────────────────────────────────────────

/// 去除口語指令詞，只保留核心主題詞
/// 例：「幫我找筆記內 飲料」→「飲料」
#[allow(dead_code)]
pub(crate) fn clean_fts_query(query: &str) -> String {
    // 前綴指令詞（依長度降序，避免短詞先匹配）
    const PREFIXES: &[&str] = &[
        "請幫我搜尋", "請幫我搜索", "請幫我查找", "請幫我找",
        "幫我搜尋", "幫我搜索", "幫我查找", "幫我找",
        "幫我", "請找", "請搜尋", "請搜索", "請查",
        "找一下", "查一下", "搜尋一下", "搜索一下",
        "搜尋", "搜索", "查找", "找找",
        "在筆記中", "在vault中", "在Vault中",
        "筆記內", "筆記裡", "筆記中",
        "vault內", "vault裡", "Vault內", "Vault裡",
        "裡面有", "裡頭有",
    ];
    // 後綴雜訊詞
    const SUFFIXES: &[&str] = &[
        "的筆記", "的資料", "的記錄", "的內容", "的相關筆記",
        "相關的筆記", "相關的資料",
    ];
    // 常見助詞/連接詞（整詞清除）
    const STOPWORDS: &[&str] = &["的", "之", "與", "和", "或", "及"];

    let mut q = query.trim().to_string();

    // 反覆剝除前綴，直到無法再匹配
    loop {
        let before = q.clone();
        for &p in PREFIXES {
            if q.starts_with(p) {
                q = q[p.len()..].trim().to_string();
                break;
            }
        }
        if q == before {
            break;
        }
    }

    // 反覆剝除後綴
    loop {
        let before = q.clone();
        for &s in SUFFIXES {
            if q.ends_with(s) {
                let end = q.len() - s.len();
                q = q[..end].trim().to_string();
                break;
            }
        }
        if q == before {
            break;
        }
    }

    // 去除純助詞開頭（如「的奶茶」→「奶茶」）
    for &w in STOPWORDS {
        if q.starts_with(w) && q.len() > w.len() {
            q = q[w.len()..].trim().to_string();
        }
    }

    if q.is_empty() {
        query.trim().to_string() // 若清洗後為空，退回原始 query
    } else {
        q
    }
}

// ── 數值比較過濾 ─────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) enum Comparison {
    LessThan(f64),
    LessThanOrEqual(f64),
    GreaterThan(f64),
    GreaterThanOrEqual(f64),
    Equal(f64),
    About(f64), // ±15%
}

#[allow(dead_code)]
impl Comparison {
    pub(crate) fn matches(&self, value: f64) -> bool {
        match self {
            Self::LessThan(v) => value < *v,
            Self::LessThanOrEqual(v) => value <= *v,
            Self::GreaterThan(v) => value > *v,
            Self::GreaterThanOrEqual(v) => value >= *v,
            Self::Equal(v) => (value - v).abs() < 0.01,
            Self::About(v) => (value - v).abs() <= v * 0.15,
        }
    }
    pub(crate) fn label(&self) -> String {
        match self {
            Self::LessThan(v) => format!("< {}", v),
            Self::LessThanOrEqual(v) => format!("≤ {}", v),
            Self::GreaterThan(v) => format!("> {}", v),
            Self::GreaterThanOrEqual(v) => format!("≥ {}", v),
            Self::Equal(v) => format!("= {}", v),
            Self::About(v) => format!("≈ {}", v),
        }
    }
}

/// 從查詢字串中解析比較詞 + 數字，返回 (比較條件, 去掉比較部分後的搜索詞)
#[allow(dead_code)]
pub(crate) fn parse_comparison(query: &str) -> (Option<Comparison>, String) {
    // 有序列表：長詞優先（避免「不超過」被「超過」先匹配）
    let keywords: &[(&str, &str)] = &[
        ("不超過", "lte"), ("不高於", "lte"), ("不大於", "lte"), ("至多", "lte"), ("最多", "lte"),
        ("不低於", "gte"), ("不小於", "gte"), ("至少", "gte"), ("最少", "gte"),
        ("低於", "lt"),  ("小於", "lt"),  ("少於", "lt"),  ("未達", "lt"),  ("不足", "lt"),
        ("高於", "gt"),  ("大於", "gt"),  ("多於", "gt"),  ("超過", "gt"),
        ("等於", "eq"),  ("剛好", "eq"),  ("恰好", "eq"),  ("正好", "eq"),
        ("大約", "about"), ("約為", "about"), ("大概", "about"),
        ("差不多", "about"), ("接近", "about"), ("約莫", "about"), ("左右", "about"),
        ("約", "about"),
    ];
    let units = ["元", "塊", "分", "度", "克", "公克", "公斤", "公升", "毫升",
                 "ml", "ML", "kg", "KG", "g", "G", "L", "km", "KM", "m", "M"];

    for &(word, kind) in keywords {
        if let Some(pos) = query.find(word) {
            let after = query[pos + word.len()..].trim_start();
            // 提取數字（整數或小數）
            let num_end = after
                .find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(after.len());
            if num_end == 0 {
                continue;
            }
            if let Ok(val) = after[..num_end].parse::<f64>() {
                let cmp = match kind {
                    "lt"    => Comparison::LessThan(val),
                    "lte"   => Comparison::LessThanOrEqual(val),
                    "gt"    => Comparison::GreaterThan(val),
                    "gte"   => Comparison::GreaterThanOrEqual(val),
                    "eq"    => Comparison::Equal(val),
                    _       => Comparison::About(val),
                };
                let before = query[..pos].trim();
                let rest = &after[num_end..];
                // 跳過單位
                let unit_skip = units
                    .iter()
                    .find(|&&u| rest.starts_with(u))
                    .map(|u| u.len())
                    .unwrap_or(0);
                // 去除助詞「的」「之」
                let after_unit = rest[unit_skip..]
                    .trim_start_matches('的')
                    .trim_start_matches('之')
                    .trim();
                let remaining = match (before.is_empty(), after_unit.is_empty()) {
                    (true,  true)  => String::new(),
                    (true,  false) => after_unit.to_string(),
                    (false, true)  => before.to_string(),
                    (false, false) => format!("{} {}", before, after_unit),
                };
                return (Some(cmp), remaining);
            }
        }
    }
    (None, query.to_string())
}

/// 從文字內容中找出含有數字且符合比較條件的行
#[allow(dead_code)]
pub(crate) fn filter_lines_by_comparison(content: &str, cmp: &Comparison) -> Vec<String> {
    let mut matched = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 掃描行內所有數字（含小數點）
        let bytes = trimmed.as_bytes();
        let mut i = 0;
        let mut found = false;
        while i < bytes.len() && !found {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                    if let Ok(val) = s.parse::<f64>() {
                        if cmp.matches(val) {
                            matched.push(trimmed.to_string());
                            found = true;
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_comparison_basic() {
        let (cmp, q) = parse_comparison("低於65元的飲料");
        assert!(matches!(cmp, Some(Comparison::LessThan(v)) if (v - 65.0).abs() < 0.01));
        assert_eq!(q, "飲料");

        let (cmp, q) = parse_comparison("高於100元的咖啡");
        assert!(matches!(cmp, Some(Comparison::GreaterThan(v)) if (v - 100.0).abs() < 0.01));
        assert_eq!(q, "咖啡");

        let (cmp, q) = parse_comparison("不超過50元的奶茶");
        assert!(matches!(cmp, Some(Comparison::LessThanOrEqual(v)) if (v - 50.0).abs() < 0.01));
        assert_eq!(q, "奶茶");

        let (cmp, q) = parse_comparison("大約30元的紅茶");
        assert!(matches!(cmp, Some(Comparison::About(v)) if (v - 30.0).abs() < 0.01));
        assert_eq!(q, "紅茶");

        let (cmp, q) = parse_comparison("至少80元的果汁");
        assert!(matches!(cmp, Some(Comparison::GreaterThanOrEqual(v)) if (v - 80.0).abs() < 0.01));
        assert_eq!(q, "果汁");
    }

    #[test]
    fn test_parse_comparison_llm_style() {
        // LLM 通常會把比較詞放前面
        let (cmp, q) = parse_comparison("飲料 低於65元");
        assert!(matches!(cmp, Some(Comparison::LessThan(v)) if (v - 65.0).abs() < 0.01));
        assert_eq!(q, "飲料");

        // 無比較詞時原樣返回
        let (cmp, q) = parse_comparison("奶茶");
        assert!(cmp.is_none());
        assert_eq!(q, "奶茶");
    }

    #[test]
    fn test_clean_fts_query() {
        // 完整口語句子 → 核心詞
        assert_eq!(clean_fts_query("幫我找筆記內低於65元的飲料"), "低於65元的飲料");
        assert_eq!(clean_fts_query("搜尋奶茶"), "奶茶");
        assert_eq!(clean_fts_query("請幫我找咖啡的筆記"), "咖啡");
        assert_eq!(clean_fts_query("在筆記中高於100元的果汁"), "高於100元的果汁");
        assert_eq!(clean_fts_query("找一下紅茶"), "紅茶");
        // 無指令詞時原樣
        assert_eq!(clean_fts_query("飲料"), "飲料");
    }

    #[test]
    fn test_full_pipeline() {
        // 模擬完整流程：口語句 → 清洗 → 解析比較 → 最終 FTS 詞
        let simulate = |raw: &str| -> (bool, String) {
            let cleaned = clean_fts_query(raw);
            let (cmp, search_query) = parse_comparison(&cleaned);
            let fts = {
                let q = if search_query.trim().is_empty() { cleaned.clone() } else { search_query.trim().to_string() };
                clean_fts_query(&q)
            };
            (cmp.is_some(), fts)
        };

        let (has_cmp, fts) = simulate("幫我找筆記內低於65元的飲料");
        assert!(has_cmp, "應解析出比較條件");
        assert_eq!(fts, "飲料");

        let (has_cmp, fts) = simulate("搜尋高於100元的咖啡");
        assert!(has_cmp);
        assert_eq!(fts, "咖啡");

        let (has_cmp, fts) = simulate("找一下奶茶");
        assert!(!has_cmp);
        assert_eq!(fts, "奶茶");
    }

    #[test]
    fn test_filter_lines_by_comparison() {
        let content = "\
珍珠奶茶 60元
抹茶拿鐵 75元
草莓奶昔 55元
黑糖鮮奶茶 65元
焦糖瑪奇朵 80元";

        let cmp = Comparison::LessThan(65.0);
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 < 65 ✓, 75 ✗, 55 < 65 ✓, 65 not < 65 ✗, 80 ✗
        assert_eq!(matched.len(), 2);
        assert!(matched[0].contains("60"));
        assert!(matched[1].contains("55"));

        let cmp = Comparison::LessThanOrEqual(65.0);
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 ✓, 55 ✓, 65 ≤ 65 ✓
        assert_eq!(matched.len(), 3);

        let cmp = Comparison::About(65.0); // ±15% → 55.25~74.75
        let matched = filter_lines_by_comparison(content, &cmp);
        // 60 ✓, 75 ✓, 55 ✗（54.25邊緣，55.25以上才算）, 65 ✓
        assert!(matched.iter().any(|l| l.contains("60")));
        assert!(matched.iter().any(|l| l.contains("65")));
    }
}
