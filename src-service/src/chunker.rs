//! Markdown Chunker — splits notes into heading-level chunks.
//!
//! Ported from src-tauri/src/vault/chunker.rs.

use once_cell::sync::Lazy;
use regex::Regex;
use sha2::{Digest, Sha256};

// ── Data types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Chunk {
    pub chunk_id:   String,
    pub file_path:  String,
    pub section:    String,   // heading text, "" = preamble
    pub content:    String,   // raw Markdown of this section
    pub word_count: usize,
    pub status:     String,   // from frontmatter: "draft" | "verified" | "deprecated" | ""
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Stable ID: first 16 hex chars of SHA-256(file_path "::" section)
fn chunk_id(file_path: &str, section: &str) -> String {
    let input = format!("{}::{}", file_path, section);
    let hash = Sha256::digest(input.as_bytes());
    format!("{:x}", hash)[..16].to_string()
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
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
/// Removes Markdown syntax, HTML tags, and compresses whitespace.
pub fn clean_for_embedding(text: &str) -> String {
    let s = RE_FRONTMATTER.replace_all(text, "");
    let s = RE_CODE_BLOCK.replace_all(&s, " ");
    let s = RE_INLINE_CODE.replace_all(&s, " ");
    let s = RE_HTML_TAG.replace_all(&s, " ");
    let s = s
        .replace("&amp;",  "&")
        .replace("&lt;",   "<")
        .replace("&gt;",   ">")
        .replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;",  "'");
    let s = RE_WIKILINK.replace_all(&s, "$1");
    let s = RE_MD_LINK.replace_all(&s, "$1");
    let s = RE_HEADING.replace_all(&s, "");
    let s = RE_BOLD_ITALIC.replace_all(&s, "$1");
    let s = RE_UNDERLINE.replace_all(&s, "$1");
    let s = RE_BLOCKQUOTE.replace_all(&s, "");
    let s = RE_LIST_MARKER.replace_all(&s, "");
    let s = RE_HR.replace_all(&s, "");
    let s = RE_MULTI_SPACE.replace_all(&s, " ");
    let s = RE_MULTI_NEWLINE.replace_all(&s, "\n\n");
    s.trim().to_string()
}

/// Parse the `status:` field from YAML frontmatter (--- ... ---)
fn parse_frontmatter_status(content: &str) -> String {
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
/// - Each heading starts a new chunk (content = from this heading until next)
/// - Empty sections are omitted
pub fn split_into_chunks(content: &str, file_path: &str) -> Vec<Chunk> {
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
            Chunk {
                chunk_id:   chunk_id(file_path, &section),
                file_path:  file_path.to_string(),
                section:    section,
                word_count: word_count(&text),
                content:    text,
                status:     status.clone(),
            }
        })
        .collect()
}
