//! Meeting-aware Agent tools.
//!
//! These tools let any Agent query the structured meeting history stored in
//! the `meetings`, `meeting_segments`, `meeting_actions`, and
//! `meeting_decisions` tables — giving cross-meeting intelligence that KB
//! (document chunks) cannot provide.
//!
//! ## Tools
//!
//! | name                     | write | description                                              |
//! |--------------------------|-------|----------------------------------------------------------|
//! | search_past_meetings     | no    | Keyword + metadata search across meeting history         |
//! | list_open_actions        | no    | Query pending action items, optionally by owner          |
//! | get_meeting_context      | no    | Full context for one meeting (meta + decisions + actions)|
//! | save_meeting_extractions | yes   | Persist structured decisions + actions from a meeting    |

use serde_json::{json, Value};
use chrono::Utc;
use crate::db::SurrealDb;

// ── search_past_meetings ──────────────────────────────────────────────────────

/// Search meeting history by keyword, optionally filtered by speaker name and
/// date range.  Returns a list of matching meetings with excerpts.
pub(crate) async fn search_past_meetings(
    db:       &SurrealDb,
    vault_id: &str,
    query:    &str,
    speaker:  Option<&str>,
    days:     Option<u64>,
    limit:    usize,
) -> Value {
    if query.is_empty() && speaker.is_none() {
        return json!({ "error": "query または speaker を指定してください" });
    }

    // Build date lower-bound (ms timestamp)
    let since_ms: Option<i64> = days.map(|d| {
        Utc::now().timestamp_millis() - (d as i64) * 86_400_000
    });

    // ── Step 1: fetch meetings in scope ──────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct MeetingRow {
        meeting_id:         String,
        started_at:         i64,
        note_path:          Option<String>,
        speaker_names_json: String,
    }

    let mut mr = if let Some(since) = since_ms {
        db.query("SELECT meeting_id, started_at, note_path, speaker_names_json FROM meetings WHERE vault_id = $vid AND started_at >= $since ORDER BY started_at DESC LIMIT 200")
            .bind(("vid", vault_id.to_string()))
            .bind(("since", since))
            .await
    } else {
        db.query("SELECT meeting_id, started_at, note_path, speaker_names_json FROM meetings WHERE vault_id = $vid ORDER BY started_at DESC LIMIT 200")
            .bind(("vid", vault_id.to_string()))
            .await
    };

    let meetings: Vec<MeetingRow> = match mr {
        Ok(ref mut r) => r.take(0).unwrap_or_default(),
        Err(e) => return json!({ "error": format!("DB error: {}", e) }),
    };

    if meetings.is_empty() {
        return json!({ "meetings": [], "total": 0 });
    }

    // ── Step 2: filter by speaker name if requested ──────────────────────────
    let speaker_lower = speaker.map(|s| s.to_lowercase());
    let candidate_ids: Vec<String> = meetings.iter()
        .filter(|m| {
            if let Some(ref spk) = speaker_lower {
                let names: std::collections::HashMap<String, String> =
                    serde_json::from_str(&m.speaker_names_json).unwrap_or_default();
                names.values().any(|n| n.to_lowercase().contains(spk.as_str()))
            } else {
                true
            }
        })
        .map(|m| m.meeting_id.clone())
        .collect();

    if candidate_ids.is_empty() {
        return json!({ "meetings": [], "total": 0,
            "note": format!("說話者 '{}' 未出現在任何會議中", speaker.unwrap_or("")) });
    }

    // ── Step 3: keyword search in segments ───────────────────────────────────
    // For each keyword (space-split), find meetings containing it in transcript.
    // If query is empty, all candidate meetings pass.
    let keywords: Vec<String> = query.split_whitespace()
        .map(|k| k.to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();

    #[derive(serde::Deserialize)]
    struct SegRow { meeting_id: String, text: String, ts_ms: i64 }

    // Fetch segments only for candidate meetings (batch, max 200 meetings)
    // We pass meeting IDs as an array filter in the query.
    let ids_json = serde_json::to_string(&candidate_ids).unwrap_or_else(|_| "[]".to_string());
    let seg_query = format!(
        "SELECT meeting_id, text, ts_ms FROM meeting_segments WHERE meeting_id INSIDE {} ORDER BY meeting_id, ts_ms LIMIT 5000",
        ids_json
    );
    let segments: Vec<SegRow> = match db.query(&seg_query).await {
        Ok(mut r) => r.take(0).unwrap_or_default(),
        Err(_) => vec![],
    };

    // Group segments by meeting_id, find matching excerpts
    let mut meeting_excerpts: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for seg in &segments {
        let text_lower = seg.text.to_lowercase();
        if keywords.is_empty() || keywords.iter().any(|k| text_lower.contains(k.as_str())) {
            meeting_excerpts.entry(seg.meeting_id.clone())
                .or_default()
                .push(seg.text.clone());
        }
    }

    // ── Step 4: build result ─────────────────────────────────────────────────
    let meta_map: std::collections::HashMap<String, &MeetingRow> =
        meetings.iter().map(|m| (m.meeting_id.clone(), m)).collect();

    let mut results: Vec<Value> = meeting_excerpts.iter()
        .filter_map(|(mid, excerpts)| {
            let m = meta_map.get(mid)?;
            let names: std::collections::HashMap<String, String> =
                serde_json::from_str(&m.speaker_names_json).unwrap_or_default();
            let participants: Vec<&String> = names.values().collect();
            let date = chrono::DateTime::from_timestamp(m.started_at / 1000, 0)
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| m.started_at.to_string());
            // Truncate excerpts to first 3, max 150 chars each
            let excerpt: String = excerpts.iter().take(3)
                .map(|e| e.chars().take(150).collect::<String>())
                .collect::<Vec<_>>()
                .join(" … ");
            Some(json!({
                "meeting_id": mid,
                "date":        date,
                "note_path":   m.note_path,
                "participants": participants,
                "excerpt":     excerpt,
            }))
        })
        .collect();

    // Sort by meeting recency (we need started_at; get from meta_map)
    results.sort_by(|a, b| {
        let ta = meta_map.get(a["meeting_id"].as_str().unwrap_or(""))
            .map(|m| m.started_at).unwrap_or(0);
        let tb = meta_map.get(b["meeting_id"].as_str().unwrap_or(""))
            .map(|m| m.started_at).unwrap_or(0);
        tb.cmp(&ta)
    });
    results.truncate(limit);

    json!({
        "meetings": results,
        "total":    results.len(),
    })
}

// ── list_open_actions ─────────────────────────────────────────────────────────

/// List open (incomplete) action items across all meetings in this vault.
/// Optionally filter by owner name substring.
pub(crate) async fn list_open_actions(
    db:       &SurrealDb,
    vault_id: &str,
    owner:    Option<&str>,
    limit:    usize,
) -> Value {
    #[derive(serde::Deserialize)]
    struct ActionRow {
        action_id:   String,
        meeting_id:  String,
        description: String,
        owner:       Option<String>,
        created_at:  i64,
    }

    let mut r = db.query(
        "SELECT action_id, meeting_id, description, owner, created_at \
         FROM meeting_actions WHERE vault_id = $vid AND status = 'open' \
         ORDER BY created_at DESC LIMIT $lim"
    )
    .bind(("vid", vault_id.to_string()))
    .bind(("lim", limit as i64))
    .await;

    let actions: Vec<ActionRow> = match r {
        Ok(ref mut q) => q.take(0).unwrap_or_default(),
        Err(e) => return json!({ "error": format!("DB error: {}", e) }),
    };

    // Filter by owner substring if requested
    let owner_lower = owner.map(|o| o.to_lowercase());
    let filtered: Vec<Value> = actions.iter()
        .filter(|a| {
            if let Some(ref o) = owner_lower {
                a.owner.as_deref().map(|n| n.to_lowercase().contains(o.as_str())).unwrap_or(false)
            } else {
                true
            }
        })
        .map(|a| {
            let date = chrono::DateTime::from_timestamp(a.created_at / 1000, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            json!({
                "action_id":   a.action_id,
                "meeting_id":  a.meeting_id,
                "description": a.description,
                "owner":       a.owner,
                "date":        date,
            })
        })
        .collect();

    json!({ "open_actions": filtered, "total": filtered.len() })
}

// ── get_meeting_context ───────────────────────────────────────────────────────

/// Return full structured context for a single meeting: metadata, participants,
/// decisions, action items, and a brief transcript preview.
pub(crate) async fn get_meeting_context(db: &SurrealDb, meeting_id: &str) -> Value {
    // ── meeting metadata ─────────────────────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct MeetingRow {
        started_at:         i64,
        ended_at:           Option<i64>,
        note_path:          Option<String>,
        speaker_names_json: String,
        language:           String,
    }
    let mut mr = match db.query(
        "SELECT started_at, ended_at, note_path, speaker_names_json, language \
         FROM meetings WHERE meeting_id = $mid LIMIT 1"
    ).bind(("mid", meeting_id.to_string())).await {
        Ok(r) => r,
        Err(e) => return json!({ "error": format!("DB error: {}", e) }),
    };
    let meeting = match mr.take::<Vec<MeetingRow>>(0).unwrap_or_default().into_iter().next() {
        Some(m) => m,
        None => return json!({ "error": format!("meeting {} not found", meeting_id) }),
    };

    let names: std::collections::HashMap<String, String> =
        serde_json::from_str(&meeting.speaker_names_json).unwrap_or_default();
    let participants: Vec<&String> = names.values().collect();
    let date = chrono::DateTime::from_timestamp(meeting.started_at / 1000, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default();
    let duration_min = meeting.ended_at.map(|e| (e - meeting.started_at) / 60_000);

    // ── decisions ────────────────────────────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct DecisionRow { description: String }
    let mut dr = db.query(
        "SELECT description FROM meeting_decisions WHERE meeting_id = $mid ORDER BY created_at"
    ).bind(("mid", meeting_id.to_string())).await;
    let decisions: Vec<String> = match dr {
        Ok(ref mut q) => q.take::<Vec<DecisionRow>>(0).unwrap_or_default()
            .into_iter().map(|r| r.description).collect(),
        Err(_) => vec![],
    };

    // ── action items ─────────────────────────────────────────────────────────
    #[derive(serde::Deserialize)]
    struct ActionRow { description: String, owner: Option<String>, status: String }
    let mut ar = db.query(
        "SELECT description, owner, status FROM meeting_actions WHERE meeting_id = $mid ORDER BY created_at"
    ).bind(("mid", meeting_id.to_string())).await;
    let actions: Vec<Value> = match ar {
        Ok(ref mut q) => q.take::<Vec<ActionRow>>(0).unwrap_or_default()
            .into_iter().map(|a| json!({
                "description": a.description,
                "owner":       a.owner,
                "status":      a.status,
            })).collect(),
        Err(_) => vec![],
    };

    // ── transcript preview (first 10 segments) ────────────────────────────────
    #[derive(serde::Deserialize)]
    struct SegRow { text: String, ts_ms: i64 }
    let mut sr = db.query(
        "SELECT text, ts_ms FROM meeting_segments WHERE meeting_id = $mid ORDER BY seg_index LIMIT 10"
    ).bind(("mid", meeting_id.to_string())).await;
    let preview: String = match sr {
        Ok(ref mut q) => q.take::<Vec<SegRow>>(0).unwrap_or_default()
            .iter().map(|s| {
                let m = s.ts_ms / 60_000;
                let sec = (s.ts_ms % 60_000) / 1000;
                format!("[{:02}:{:02}] {}", m, sec, s.text)
            }).collect::<Vec<_>>().join("\n"),
        Err(_) => String::new(),
    };

    json!({
        "meeting_id":   meeting_id,
        "date":         date,
        "language":     meeting.language,
        "duration_min": duration_min,
        "participants": participants,
        "note_path":    meeting.note_path,
        "decisions":    decisions,
        "actions":      actions,
        "transcript_preview": preview,
    })
}

// ── save_meeting_extractions ──────────────────────────────────────────────────

/// Persist structured decisions and action items extracted by the Agent.
/// Called by the meeting_summarizer Agent after generating the note.
///
/// `decisions`: array of strings (each is one decision statement)
/// `actions`:   array of objects `{ description: string, owner?: string }`
pub(crate) async fn save_meeting_extractions(
    db:         &SurrealDb,
    vault_id:   &str,
    meeting_id: &str,
    decisions:  &[Value],
    actions:    &[Value],
) -> Value {
    let now = Utc::now().timestamp_millis();

    // ── Write decisions ──────────────────────────────────────────────────────
    let mut saved_decisions = 0usize;
    for (i, d) in decisions.iter().enumerate() {
        let desc = match d.as_str().or_else(|| d["description"].as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => continue,
        };
        let did = format!("{}-d{}", &meeting_id[..meeting_id.len().min(8)], i);
        let _ = db.query(
            "INSERT INTO meeting_decisions (decision_id, meeting_id, vault_id, description, created_at) \
             VALUES ($did, $mid, $vid, $desc, $now) ON DUPLICATE KEY UPDATE description = $desc"
        )
        .bind(("did",  did))
        .bind(("mid",  meeting_id.to_string()))
        .bind(("vid",  if vault_id.is_empty() { None::<String> } else { Some(vault_id.to_string()) }))
        .bind(("desc", desc))
        .bind(("now",  now))
        .await;
        saved_decisions += 1;
    }

    // ── Write action items ───────────────────────────────────────────────────
    let mut saved_actions = 0usize;
    for (i, a) in actions.iter().enumerate() {
        let desc = match a.as_str().or_else(|| a["description"].as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => continue,
        };
        let owner = a["owner"].as_str().map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != "TBD");
        let aid = format!("{}-a{}", &meeting_id[..meeting_id.len().min(8)], i);
        let _ = db.query(
            "INSERT INTO meeting_actions (action_id, meeting_id, vault_id, description, owner, status, created_at) \
             VALUES ($aid, $mid, $vid, $desc, $owner, 'open', $now) ON DUPLICATE KEY UPDATE description = $desc, owner = $owner"
        )
        .bind(("aid",   aid))
        .bind(("mid",   meeting_id.to_string()))
        .bind(("vid",   if vault_id.is_empty() { None::<String> } else { Some(vault_id.to_string()) }))
        .bind(("desc",  desc))
        .bind(("owner", owner))
        .bind(("now",   now))
        .await;
        saved_actions += 1;
    }

    json!({
        "ok":              true,
        "saved_decisions": saved_decisions,
        "saved_actions":   saved_actions,
    })
}

// ── complete_action ───────────────────────────────────────────────────────────

/// Mark an action item as done.
pub(crate) async fn complete_action(db: &SurrealDb, action_id: &str) -> Value {
    let _ = db.query(
        "UPDATE meeting_actions SET status = 'done' WHERE action_id = $aid"
    ).bind(("aid", action_id.to_string())).await;
    json!({ "ok": true, "action_id": action_id, "status": "done" })
}
