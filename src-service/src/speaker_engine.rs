//! SpeakerEngine — sole owner of speaker profiles and CAM++ enrollment.
//!
//! # Ownership rules
//! - **Only** this module calls `EmbedModel::extract` + profile enrollment.
//!   `ws_transcribe` (TranscriptionEngine) never touches CAM++ or speaker profiles.
//! - Reads PCM via `AudioStore::slice()` — never touches raw PCM buffers directly.
//! - Writes speaker attribution to `speaker_spans` DB table only.
//!   Never updates `meeting_segments.speaker`.
//!
//! # Event channel (single ordered mpsc)
//! ```
//! SegmentReady(seg) → accumulate buffer
//! MeetingEnd        → flush buffer, process, emit AttributionComplete
//! ```
//! MeetingEnd is enqueued **after** all SegmentReady events by the caller (ws_transcribe),
//! which guarantees correct drain order without any mutex.
//!
//! # Modes
//! - Bootstrap: accumulate MIN_BOOTSTRAP_SEGMENTS, then K-means cluster by embedding.
//!   K is chosen via silhouette score (k=1..MAX_K).
//! - Normal: batch N segments → LLM FindBreakpoints → CAM++ verify → write speaker_spans.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use serde_json::json;

use crate::app_state::ApiState;
use crate::audio_store::AudioStore;
use crate::diarize::EmbedModel;
use crate::routes::whisper::WordTimestamp;

// ── Tuning constants ──────────────────────────────────────────────────────────

/// Minimum segments before bootstrap K-means can run.
const MIN_BOOTSTRAP_SEGMENTS: usize = 5;
/// Maximum number of distinct speakers we attempt to discover.
const MAX_SPEAKERS: usize = 6;
/// How many unverified segments accumulate before triggering a P2 batch.
const BATCH_SIZE: usize = 4;
/// Tail text (chars) from last batch to include as LLM prefix context.
const PREFIX_CONTEXT_CHARS: usize = 200;
/// CAM++ slice window around each word boundary (ms).
const SLICE_WINDOW_MS: u64 = 1500;
/// Minimum audio (ms) needed for a reliable CAM++ embedding.
const MIN_EMBED_MS: u64 = 1000;

// ── Public types ──────────────────────────────────────────────────────────────

/// Segment data passed from TranscriptionEngine → SpeakerEngine.
#[derive(Debug, Clone)]
pub struct SegmentForAttribution {
    pub seg_id: String,
    pub meeting_id: String,
    pub seg_index: u32,
    pub text: String,
    pub words: Vec<WordTimestamp>,
    /// Absolute ms from meeting start (based on total_samples_received counter).
    pub chunk_start_ms: u64,
}

/// Events flowing into SpeakerEngine's input channel (single ordered mpsc).
pub enum SpeakerEvent {
    SegmentReady(SegmentForAttribution),
    /// Flush remaining buffer, process, then signal completion via the oneshot sender.
    MeetingEnd(oneshot::Sender<()>),
}

// ── Speaker profile (in-memory) ───────────────────────────────────────────────

struct SpeakerProfile {
    label: String,    // "SPEAKER_00", "SPEAKER_01", …
    embedding: Vec<f32>,
    count: u32,
}

// ── Engine ────────────────────────────────────────────────────────────────────

struct SpeakerEngineInner {
    state: ApiState,
    audio_store: AudioStore,
    model: Arc<EmbedModel>,
    profiles: Vec<SpeakerProfile>,
    next_speaker_id: u32,
    buffer: Vec<SegmentForAttribution>,
    bootstrapped: bool,
    /// Tail text of the last processed batch — sent as prefix_context to LLM.
    last_batch_tail: Option<String>,
}

impl SpeakerEngineInner {
    fn new(state: ApiState, audio_store: AudioStore, model: Arc<EmbedModel>) -> Self {
        Self {
            state,
            audio_store,
            model,
            profiles: Vec::new(),
            next_speaker_id: 0,
            buffer: Vec::new(),
            bootstrapped: false,
            last_batch_tail: None,
        }
    }

    // ── Profile helpers ───────────────────────────────────────────────────────

    fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    fn l2_norm(v: &mut Vec<f32>) {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-8 { v.iter_mut().for_each(|x| *x /= norm); }
    }

    /// Find best matching profile or create a new one. Returns the speaker label.
    fn assign_speaker(&mut self, embedding: &[f32]) -> String {
        if self.profiles.is_empty() {
            return self.new_profile(embedding);
        }
        let (best_idx, best_sim) = self.profiles.iter().enumerate()
            .map(|(i, p)| (i, Self::cosine_sim(embedding, &p.embedding)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();

        if best_sim >= 0.65 {
            // Confident match — EMA update
            let p = &mut self.profiles[best_idx];
            let alpha = 0.15f32;
            for (e, n) in p.embedding.iter_mut().zip(embedding.iter()) {
                *e = (1.0 - alpha) * *e + alpha * n;
            }
            Self::l2_norm(&mut self.profiles[best_idx].embedding);
            self.profiles[best_idx].count += 1;
            self.profiles[best_idx].label.clone()
        } else if best_sim < 0.45 && self.profiles.len() < MAX_SPEAKERS {
            self.new_profile(embedding)
        } else {
            self.profiles[best_idx].label.clone()
        }
    }

    fn new_profile(&mut self, embedding: &[f32]) -> String {
        let label = format!("SPEAKER_{:02}", self.next_speaker_id);
        self.next_speaker_id += 1;
        let mut emb = embedding.to_vec();
        Self::l2_norm(&mut emb);
        self.profiles.push(SpeakerProfile { label: label.clone(), embedding: emb, count: 1 });
        label
    }

    // ── Embedding helpers ─────────────────────────────────────────────────────

    /// Extract embedding from a PCM slice. Returns None if audio is too short or extraction fails.
    fn embed_segment(&self, samples: &[i16]) -> Option<Vec<f32>> {
        if samples.len() < (MIN_EMBED_MS * 16) as usize { return None; }
        match self.model.extract(samples) {
            Ok(mut emb) => { Self::l2_norm_static(&mut emb); Some(emb) }
            Err(_) => None,
        }
    }

    fn l2_norm_static(v: &mut Vec<f32>) {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-8 { v.iter_mut().for_each(|x| *x /= norm); }
    }

    /// Get the absolute start_ms and end_ms of a segment's audio.
    fn segment_time_range(seg: &SegmentForAttribution) -> (u64, u64) {
        if seg.words.is_empty() {
            // No word timestamps; use a 3s window starting at chunk_start_ms
            return (seg.chunk_start_ms, seg.chunk_start_ms + 3000);
        }
        let start = seg.chunk_start_ms + (seg.words.first().unwrap().start_s * 1000.0) as u64;
        let end   = seg.chunk_start_ms + (seg.words.last().unwrap().end_s   * 1000.0) as u64;
        (start, end)
    }

    // ── Bootstrap ─────────────────────────────────────────────────────────────

    /// Run K-means bootstrap on accumulated buffer. Selects k via silhouette score.
    async fn run_bootstrap(&mut self) {
        let mut embeddings: Vec<(usize, Vec<f32>)> = Vec::new(); // (buffer_idx, embedding)

        for (i, seg) in self.buffer.iter().enumerate() {
            let (start_ms, end_ms) = Self::segment_time_range(seg);
            let samples = self.audio_store.slice(start_ms, end_ms);
            if let Some(emb) = self.embed_segment(&samples) {
                embeddings.push((i, emb));
            }
        }

        if embeddings.is_empty() {
            self.bootstrapped = true;
            return;
        }

        // If we only got embeddings for 1 segment, single speaker assumed.
        if embeddings.len() == 1 {
            let label = self.new_profile(&embeddings[0].1);
            let seg = &self.buffer[embeddings[0].0];
            let (start_ms, end_ms) = Self::segment_time_range(seg);
            self.write_span(&seg.meeting_id, &label, start_ms, end_ms).await;
            self.bootstrapped = true;
            return;
        }

        // Select best k (1..=min(MAX_SPEAKERS, n_embeddings))
        let best_k = select_k_silhouette(&embeddings.iter().map(|(_, e)| e.as_slice()).collect::<Vec<_>>(), MAX_SPEAKERS);
        let assignments = kmeans(&embeddings.iter().map(|(_, e)| e.as_slice()).collect::<Vec<_>>(), best_k);

        // Build profiles from cluster centroids
        let mut cluster_embs: Vec<Vec<Vec<f32>>> = vec![Vec::new(); best_k];
        for (local_i, (buf_i, emb)) in embeddings.iter().enumerate() {
            let _ = buf_i;
            cluster_embs[assignments[local_i]].push(emb.clone());
        }

        // Create profile per cluster (centroid)
        let mut cluster_labels: Vec<String> = Vec::new();
        for cluster in &cluster_embs {
            let centroid = mean_embedding(cluster);
            cluster_labels.push(self.new_profile(&centroid));
        }

        // Write speaker_spans for each bootstrap segment
        for (local_i, (buf_i, _)) in embeddings.iter().enumerate() {
            let seg = &self.buffer[*buf_i];
            let label = &cluster_labels[assignments[local_i]];
            let (start_ms, end_ms) = Self::segment_time_range(seg);
            self.write_span(&seg.meeting_id, label, start_ms, end_ms).await;
        }

        self.bootstrapped = true;
        tracing::info!("SpeakerEngine bootstrap complete: {} speakers, {} profiles", best_k, self.profiles.len());
    }

    // ── Normal mode batch ─────────────────────────────────────────────────────

    async fn process_batch(&mut self) {
        if self.buffer.is_empty() { return; }

        // Sort by chunk_start_ms to guarantee chronological order
        // (TranscriptionEngine may emit out of order due to parallel Whisper tasks)
        self.buffer.sort_by_key(|s| s.chunk_start_ms);

        let meeting_id = self.buffer[0].meeting_id.clone();

        // Build timestamped transcript for LLM.
        // Each word is prefixed with its absolute ms so the LLM can return precise breakpoints.
        // Format: [12345ms] word  [12600ms] word ...
        let timed_transcript = build_timed_transcript(&self.buffer);

        // Get batch absolute time range
        let batch_start_ms = {
            let seg = &self.buffer[0];
            Self::segment_time_range(seg).0
        };
        let batch_end_ms = {
            let seg = self.buffer.last().unwrap();
            Self::segment_time_range(seg).1
        };

        // Call LLM to find speaker-change breakpoints
        let breakpoints = self.find_breakpoints(&timed_transcript, batch_start_ms, batch_end_ms).await;

        // Update last_batch_tail for next batch's prefix context.
        // Use plain text (no timestamps) so the LLM understands the conversation flow.
        let plain_text: String = self.buffer.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ");
        self.last_batch_tail = Some(
            plain_text.chars()
                .rev()
                .take(PREFIX_CONTEXT_CHARS)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        );

        // Build time regions from breakpoints
        let mut regions: Vec<(u64, u64)> = Vec::new();
        let mut prev = batch_start_ms;
        for &bp in &breakpoints {
            if bp > prev && bp < batch_end_ms {
                regions.push((prev, bp));
                prev = bp;
            }
        }
        regions.push((prev, batch_end_ms));

        // For each region: embed PCM → assign speaker → write span
        for (start_ms, end_ms) in regions {
            let samples = self.audio_store.slice(start_ms, end_ms);
            let speaker = if let Some(emb) = self.embed_segment(&samples) {
                self.assign_speaker(&emb)
            } else {
                // Not enough audio — assign based on nearest neighbour of word embeddings
                self.profiles.first().map(|p| p.label.clone())
                    .unwrap_or_else(|| self.new_profile(&vec![0.0; 192]))
            };
            self.write_span(&meeting_id, &speaker, start_ms, end_ms).await;
        }

        self.buffer.clear();
    }

    // ── LLM FindBreakpoints ───────────────────────────────────────────────────

    /// Ask LLM to find speaker-change timestamps in the transcript.
    /// `timed_transcript` contains word-level timestamps so the LLM can return precise ms values.
    /// Returns sorted list of absolute_ms breakpoints (may be empty).
    async fn find_breakpoints(&self, timed_transcript: &str, batch_start_ms: u64, batch_end_ms: u64) -> Vec<u64> {
        let prefix = self.last_batch_tail.as_deref().unwrap_or("");
        let prefix_block = if prefix.is_empty() {
            String::new()
        } else {
            format!("[前段對話尾部，僅供上下文參考]\n{}\n\n", prefix)
        };

        let prompt = format!(
            "{}[本批次逐字稿，每個詞前標有絕對時間（毫秒），時間範圍 {}ms–{}ms]\n\
            {}\n\n\
            請根據說話內容判斷說話者切換的位置，找出切換發生的詞的絕對時間（毫秒）。\
            只輸出 JSON 陣列，格式：[毫秒數, ...]。若沒有切換則輸出 []。只輸出 JSON，不要其他文字。",
            prefix_block, batch_start_ms, batch_end_ms, timed_transcript
        );

        let base_url = &self.state.daemon.llm_url;
        let result = self.state.daemon.http_client
            .post(format!("{}/v1/chat/completions", base_url))
            .json(&json!({
                "messages": [
                    {"role": "system", "content": "你是一個語者切換分析助理，根據逐字稿找出說話者切換的絕對時間點（毫秒）。"},
                    {"role": "user", "content": prompt},
                ],
                "max_tokens": 256,
                "temperature": 0.0,
                "stream": false,
            }))
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await;

        let text = match result {
            Ok(resp) if resp.status().is_success() => {
                let json: serde_json::Value = resp.json().await.unwrap_or_default();
                json["choices"][0]["message"]["content"]
                    .as_str().unwrap_or("[]").trim().to_string()
            }
            _ => return vec![],
        };

        // Parse JSON array of ms values
        serde_json::from_str::<Vec<serde_json::Value>>(&text)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_u64())
            .filter(|&ms| ms > 0)
            .collect()
    }

    // ── DB write ──────────────────────────────────────────────────────────────

    async fn write_span(&self, meeting_id: &str, speaker_id: &str, start_ms: u64, end_ms: u64) {
        let span_id = format!("{}-{}-{}", meeting_id, speaker_id, start_ms);
        let _ = self.state.db
            .query("INSERT INTO speaker_spans (span_id, meeting_id, speaker_id, start_ms, end_ms) VALUES ($sid, $mid, $spk, $sms, $ems)")
            .bind(("sid", span_id))
            .bind(("mid", meeting_id.to_string()))
            .bind(("spk", speaker_id.to_string()))
            .bind(("sms", start_ms as i64))
            .bind(("ems", end_ms   as i64))
            .await;

        // Emit real-time update to connected WebSocket clients
        self.state.daemon.emit("speaker:span_added", json!({
            "meeting_id": meeting_id,
            "speaker_id": speaker_id,
            "start_ms": start_ms,
            "end_ms": end_ms,
        }));
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Spawn the SpeakerEngine background task.
///
/// Returns the input channel sender. Caller enqueues `SegmentReady` events and
/// eventually a `MeetingEnd` event. The `MeetingEnd` event carries a oneshot sender
/// that fires when all attribution is complete (safe to start PostProcess).
pub fn spawn(
    state: ApiState,
    audio_store: AudioStore,
    model: Arc<EmbedModel>,
) -> mpsc::Sender<SpeakerEvent> {
    let (tx, mut rx) = mpsc::channel::<SpeakerEvent>(64);

    tokio::spawn(async move {
        let mut engine = SpeakerEngineInner::new(state, audio_store, model);

        loop {
            match rx.recv().await {
                None => break, // channel closed

                Some(SpeakerEvent::SegmentReady(seg)) => {
                    engine.buffer.push(seg);

                    // Bootstrap: wait until we have enough segments
                    if !engine.bootstrapped {
                        if engine.buffer.len() >= MIN_BOOTSTRAP_SEGMENTS {
                            engine.run_bootstrap().await;
                            // Bootstrap consumed the buffer — clear it
                            engine.buffer.clear();
                        }
                        // else: keep accumulating
                        continue;
                    }

                    // Normal mode: process when batch is full
                    if engine.buffer.len() >= BATCH_SIZE {
                        engine.process_batch().await;
                    }
                }

                Some(SpeakerEvent::MeetingEnd(done_tx)) => {
                    // Flush remaining buffer (partial batch or incomplete bootstrap)
                    if !engine.buffer.is_empty() {
                        if !engine.bootstrapped {
                            // Short meeting: bootstrap with whatever we have
                            if engine.buffer.len() >= 2 {
                                engine.run_bootstrap().await;
                            }
                            // If < 2 segments, skip attribution entirely
                        } else {
                            engine.process_batch().await;
                        }
                        engine.buffer.clear();
                    }

                    let _ = done_tx.send(());
                    break;
                }
            }
        }
    });

    tx
}

// ── Transcript helpers ────────────────────────────────────────────────────────

/// Build a timestamped transcript string for the LLM FindBreakpoints prompt.
///
/// Each word is prefixed with its absolute ms timestamp so the LLM can return
/// precise breakpoint positions. Falls back to segment-level timestamps when
/// word-level data is unavailable.
///
/// Example output:
/// `[60000ms] 我覺得 [60350ms] 這個方案 [61200ms] 好我同意`
fn build_timed_transcript(buffer: &[SegmentForAttribution]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for seg in buffer {
        if seg.words.is_empty() {
            // No word timestamps: emit segment as a single timestamped block
            parts.push(format!("[{}ms] {}", seg.chunk_start_ms, seg.text));
        } else {
            for w in &seg.words {
                let abs_ms = seg.chunk_start_ms + (w.start_s * 1000.0) as u64;
                parts.push(format!("[{}ms] {}", abs_ms, w.word.trim()));
            }
        }
    }
    parts.join(" ")
}

// ── K-means + Silhouette ──────────────────────────────────────────────────────

fn cosine_sim_slice(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>().clamp(-1.0, 1.0)
}

fn cosine_dist(a: &[f32], b: &[f32]) -> f32 {
    1.0 - cosine_sim_slice(a, b)
}

fn mean_embedding(vecs: &[Vec<f32>]) -> Vec<f32> {
    if vecs.is_empty() { return vec![]; }
    let dim = vecs[0].len();
    let mut sum = vec![0.0f32; dim];
    for v in vecs { for (s, x) in sum.iter_mut().zip(v.iter()) { *s += x; } }
    let n = vecs.len() as f32;
    let mut mean: Vec<f32> = sum.iter().map(|x| x / n).collect();
    let norm = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 { mean.iter_mut().for_each(|x| *x /= norm); }
    mean
}

/// Simple Lloyd's K-means with cosine distance, 20 iterations.
/// Returns cluster assignments (index per point).
fn kmeans(points: &[&[f32]], k: usize) -> Vec<usize> {
    let n = points.len();
    if k >= n { return (0..n).collect(); }

    // Initialise centroids with k-means++ style (spread out)
    let mut centroids: Vec<Vec<f32>> = vec![points[0].to_vec()];
    for _ in 1..k {
        let dists: Vec<f32> = points.iter()
            .map(|p| centroids.iter().map(|c| cosine_dist(p, c)).fold(f32::MAX, f32::min))
            .collect();
        let total: f32 = dists.iter().sum();
        let mut r = (rand_f32() * total).min(total - 1e-6);
        let mut chosen = 0;
        for (i, &d) in dists.iter().enumerate() { r -= d; if r <= 0.0 { chosen = i; break; } }
        centroids.push(points[chosen].to_vec());
    }

    let mut assignments = vec![0usize; n];
    for _ in 0..20 {
        // Assign
        for (i, p) in points.iter().enumerate() {
            assignments[i] = centroids.iter().enumerate()
                .map(|(ci, c)| (ci, cosine_dist(p, c)))
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
                .map(|(ci, _)| ci)
                .unwrap_or(0);
        }
        // Update centroids
        for ci in 0..k {
            let members: Vec<Vec<f32>> = points.iter().enumerate()
                .filter(|(i, _)| assignments[*i] == ci)
                .map(|(_, p)| p.to_vec())
                .collect();
            if !members.is_empty() { centroids[ci] = mean_embedding(&members); }
        }
    }
    assignments
}

/// Choose k that maximises the mean silhouette coefficient (k = 1..=max_k).
fn select_k_silhouette(points: &[&[f32]], max_k: usize) -> usize {
    let n = points.len();
    let max_k = max_k.min(n);
    if max_k <= 1 || n <= 2 { return 1; }

    let mut best_k = 1;
    let mut best_score = f32::NEG_INFINITY;

    for k in 2..=max_k {
        let assignments = kmeans(points, k);
        let score = silhouette(points, &assignments, k);
        if score > best_score { best_score = score; best_k = k; }
    }

    // Only split if silhouette is meaningfully better than single cluster
    if best_score < 0.15 { 1 } else { best_k }
}

fn silhouette(points: &[&[f32]], assignments: &[usize], k: usize) -> f32 {
    let n = points.len();
    if n <= 1 { return 0.0; }

    let scores: Vec<f32> = (0..n).map(|i| {
        let ci = assignments[i];
        // a(i): mean dist to same cluster
        let same: Vec<f32> = (0..n).filter(|&j| j != i && assignments[j] == ci)
            .map(|j| cosine_dist(points[i], points[j])).collect();
        let a = if same.is_empty() { 0.0 } else { same.iter().sum::<f32>() / same.len() as f32 };
        // b(i): min mean dist to any other cluster
        let b = (0..k).filter(|&c| c != ci).map(|c| {
            let others: Vec<f32> = (0..n).filter(|&j| assignments[j] == c)
                .map(|j| cosine_dist(points[i], points[j])).collect();
            if others.is_empty() { f32::MAX } else { others.iter().sum::<f32>() / others.len() as f32 }
        }).fold(f32::MAX, f32::min);
        if b == f32::MAX { 0.0 } else { (b - a) / a.max(b) }
    }).collect();

    scores.iter().sum::<f32>() / n as f32
}

/// Minimal pseudo-random f32 in [0,1) using a simple xorshift64 seeded from SystemTime.
/// Called only during bootstrap K-means++ (≤ MAX_SPEAKERS iterations), so state
/// does not need to be shared — each call seeds a fresh LCG from the current timestamp.
fn rand_f32() -> f32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xdeadbeef);
    // xorshift64
    let mut x = seed ^ (seed << 13);
    x ^= x >> 7;
    x ^= x << 17;
    // Map high 24 bits to [0, 1)
    ((x >> 40) as f32) / (1u64 << 24) as f32
}
