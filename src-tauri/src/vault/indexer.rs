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

/// 解析 YAML frontmatter 中的 tags
pub fn parse_frontmatter_tags(content: &str) -> Vec<String> {
    if !content.starts_with("---") {
        return vec![];
    }
    let end = match content[3..].find("\n---") {
        Some(e) => e + 3,
        None => return vec![],
    };
    let frontmatter = &content[3..end];

    let mut tags = Vec::new();
    let mut in_tags = false;

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("tags:") {
            let inline = trimmed["tags:".len()..].trim();
            if inline.starts_with('[') {
                // inline array: tags: [tag1, tag2]
                let inner = inline.trim_matches(|c| c == '[' || c == ']');
                for t in inner.split(',') {
                    let tag = t.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !tag.is_empty() {
                        tags.push(tag);
                    }
                }
            } else if inline.is_empty() {
                in_tags = true;
            }
        } else if in_tags {
            if trimmed.starts_with('-') {
                let tag = trimmed[1..].trim().trim_matches('"').trim_matches('\'').to_string();
                if !tag.is_empty() {
                    tags.push(tag);
                }
            } else if !trimmed.is_empty() {
                in_tags = false;
            }
        }
    }

    tags
}
