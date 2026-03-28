#![allow(dead_code)]
use regex::Regex;
use once_cell::sync::Lazy;

#[derive(Debug, Clone)]
pub struct ParsedLink {
    pub raw_text: String,
    pub target_title: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    pub link_type: String,
    pub line_number: i64,
}

/// 解析 Markdown 內容中的所有 wikilinks 和 image embeds
pub fn parse_links(content: &str) -> Vec<ParsedLink> {
    static WIKILINK_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"!?\[\[([^\[\]]+)\]\]").unwrap()
    });

    let mut links = Vec::new();

    for (line_idx, line) in content.lines().enumerate() {
        for cap in WIKILINK_RE.captures_iter(line) {
            let raw_text = cap[0].to_string();
            let inner = cap[1].to_string();
            let is_embed = raw_text.starts_with('!');

            // 解析 [[title#heading|alias]] 格式
            let (title_part, alias) = if let Some(pipe_pos) = inner.find('|') {
                (inner[..pipe_pos].to_string(), Some(inner[pipe_pos + 1..].to_string()))
            } else {
                (inner.clone(), None)
            };

            let (target_title, heading) = if let Some(hash_pos) = title_part.find('#') {
                (
                    title_part[..hash_pos].to_string(),
                    Some(title_part[hash_pos + 1..].to_string()),
                )
            } else {
                (title_part, None)
            };

            // 判斷是圖片嵌入還是 wikilink
            let link_type = if is_embed {
                let lower = target_title.to_lowercase();
                if lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")
                    || lower.ends_with(".gif") || lower.ends_with(".webp") || lower.ends_with(".svg")
                {
                    "image_embed"
                } else {
                    "wikilink"
                }
            } else {
                "wikilink"
            };

            links.push(ParsedLink {
                raw_text,
                target_title: target_title.trim().to_string(),
                alias,
                heading,
                link_type: link_type.to_string(),
                line_number: line_idx as i64,
            });
        }
    }

    links
}
