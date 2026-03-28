//! Markdown Chunker — splits notes into heading-level chunks.
#![allow(dead_code)]

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};

// ── Data types ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Chunk {
    pub id:         String,
    pub file_path:  String,
    pub section:    String,   // heading text, "" = preamble
    pub content:    String,   // raw Markdown of this section
    pub links:      Vec<String>, // wikilink target titles found in content
    pub chunk_type: String,
    pub word_count: i64,
    #[allow(dead_code)]
    pub updated_at: i64,      // milliseconds
    #[allow(dead_code)]
    pub embedding:  Option<Vec<f32>>,
    pub status:     String,   // from frontmatter: "draft" | "verified" | "deprecated" | ""
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Stable ID: first 16 hex chars of SHA-256(file_path "::" section)
fn chunk_id(file_path: &str, section: &str) -> String {
    let input = format!("{}::{}", file_path, section);
    let hash = Sha256::digest(input.as_bytes());
    format!("{:x}", hash)[..16].to_string()
}

/// Extract all [[wikilink]] target titles (ignores alias and heading suffix)
fn extract_wikilinks(text: &str) -> Vec<String> {
    let re = Regex::new(r"\[\[([^\]|#\n]+)(?:[|#][^\]]*)?]]").unwrap();
    re.captures_iter(text)
        .map(|c| c[1].trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn word_count(text: &str) -> i64 {
    text.split_whitespace().count() as i64
}

// ── Embedding pre-processing ───────────────────────────────────────────────

static RE_FRONTMATTER:  Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)^---\r?\n.*?\n---\r?\n?").unwrap());
static RE_CODE_BLOCK:   Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)```[^\n]*\n.*?```").unwrap());
static RE_INLINE_CODE:  Lazy<Regex> = Lazy::new(|| Regex::new(r"`[^`\n]+`").unwrap());
static RE_HTML_TAG:     Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());
static RE_WIKILINK:     Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\[([^\]|#]+)(?:[|#][^\]]*)?\]\]").unwrap());
static RE_MD_LINK:      Lazy<Regex> = Lazy::new(|| Regex::new(r"\[([^\]]*)\]\([^\)]*\)").unwrap());
static RE_HEADING:      Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^#{1,6}\s+").unwrap());
static RE_BOLD_ITALIC:  Lazy<Regex> = Lazy::new(|| Regex::new(r"\*{1,3}([^*\n]+)\*{1,3}").unwrap());
static RE_UNDERLINE:    Lazy<Regex> = Lazy::new(|| Regex::new(r"_{1,3}([^_\n]+)_{1,3}").unwrap());
static RE_BLOCKQUOTE:   Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^>\s?").unwrap());
static RE_LIST_MARKER:  Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^[-*+]\s+|^\d+\.\s+").unwrap());
static RE_HR:           Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^[-*_]{3,}\s*$").unwrap());
static RE_MULTI_SPACE:  Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\S\n]+").unwrap());
static RE_MULTI_NEWLINE:Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());

/// Clean chunk content before sending to the embedding server.
///
/// Removes Markdown syntax, HTML tags, and compresses whitespace.
/// **Does NOT strip punctuation** (punctuation carries semantic meaning in CJK).
/// The original `content` stored in DB is never modified — this is only applied
/// to the text passed to the embedding model.
pub fn clean_for_embedding(text: &str) -> String {
    // 1. Strip YAML frontmatter
    let s = RE_FRONTMATTER.replace_all(text, "");
    // 2. Remove fenced code blocks (keep a space so surrounding words don't merge)
    let s = RE_CODE_BLOCK.replace_all(&s, " ");
    // 3. Remove inline code
    let s = RE_INLINE_CODE.replace_all(&s, " ");
    // 4. Remove HTML tags
    let s = RE_HTML_TAG.replace_all(&s, " ");
    // 5. Decode common HTML entities
    let s = s
        .replace("&amp;",  "&")
        .replace("&lt;",   "<")
        .replace("&gt;",   ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;",  "'");
    // 6. Wikilinks [[text|alias]] → text
    let s = RE_WIKILINK.replace_all(&s, "$1");
    // 7. Markdown links [text](url) → text
    let s = RE_MD_LINK.replace_all(&s, "$1");
    // 8. Heading markers (## Title → Title)
    let s = RE_HEADING.replace_all(&s, "");
    // 9. Bold / italic markers (**text** → text)
    let s = RE_BOLD_ITALIC.replace_all(&s, "$1");
    let s = RE_UNDERLINE.replace_all(&s, "$1");
    // 10. Blockquote markers
    let s = RE_BLOCKQUOTE.replace_all(&s, "");
    // 11. List markers (- item / 1. item)
    let s = RE_LIST_MARKER.replace_all(&s, "");
    // 12. Horizontal rules
    let s = RE_HR.replace_all(&s, "");
    // 13. Compress non-newline whitespace → single space
    let s = RE_MULTI_SPACE.replace_all(&s, " ");
    // 14. Compress 3+ consecutive newlines → 2
    let s = RE_MULTI_NEWLINE.replace_all(&s, "\n\n");

    s.trim().to_string()
}

/// Parse the `status:` field from YAML frontmatter (--- ... ---)
pub fn parse_frontmatter_status(content: &str) -> String {
    if !content.starts_with("---") { return String::new(); }
    let after = if content.starts_with("---\r\n") { 5 } else { 4 };
    let end = match content[after..].find("\n---") {
        Some(i) => after + i,
        None => return String::new(),
    };
    for line in content[after..end].lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("status:") {
            let val = trimmed["status:".len()..].trim();
            if matches!(val, "verified" | "draft" | "deprecated") {
                return val.to_string();
            }
        }
    }
    String::new()
}

// ── Public API ────────────────────────────────────────────────────────────

/// Split a Markdown note into heading-level chunks.
///
/// Strategy:
/// - Text before the first heading → chunk with section = ""
/// - Each heading starts a new chunk (content = from this heading until next
///   heading of same or higher level)
/// - Empty sections are omitted
pub fn chunk_note(file_path: &str, content: &str, now_ms: i64) -> Vec<Chunk> {
    let status = parse_frontmatter_status(content);
    let heading_re = Regex::new(r"(?m)^(#{1,6}) (.+)$").unwrap();

    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut current_section = String::new(); // "" = preamble
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        if let Some(caps) = heading_re.captures(line) {
            let section_name = caps[2].trim().to_string();
            let body = current_lines.join("\n").trim().to_string();
            if !body.is_empty() {
                sections.push((current_section.clone(), current_lines.clone()));
            }
            current_section = section_name;
            current_lines = vec![line];
        } else {
            current_lines.push(line);
        }
    }
    // last section
    let body = current_lines.join("\n").trim().to_string();
    if !body.is_empty() {
        sections.push((current_section, current_lines));
    }

    sections
        .into_iter()
        .map(|(section, lines)| {
            let text = lines.join("\n").trim().to_string();
            let links = extract_wikilinks(&text);
            Chunk {
                id:         chunk_id(file_path, &section),
                file_path:  file_path.to_string(),
                section:    section,
                content:    text.clone(),
                links,
                chunk_type: "text".to_string(),
                word_count: word_count(&text),
                updated_at: now_ms,
                embedding:  None,
                status:     status.clone(),
            }
        })
        .collect()
}

