/// Online speaker tracker using cosine-similarity nearest-neighbour assignment
/// with hysteresis thresholds and EMA (exponential moving average) embedding update.
///
/// # Thresholds
/// - `sim >= HIGH` → definitely the same speaker → assign + update EMA
/// - `sim <  LOW`  → definitely different → new speaker (if under max_speakers)
/// - `LOW <= sim < HIGH` → uncertain → assign best match but do NOT update EMA
///
/// This prevents EMA drift when the model is unsure.
pub struct SpeakerTracker {
    profiles: Vec<SpeakerProfile>,
    high_threshold: f32,
    low_threshold: f32,
    max_speakers: usize,
    next_id: u32,
}

struct SpeakerProfile {
    label: String,
    /// L2-normalised EMA embedding
    embedding: Vec<f32>,
    count: u32,
}

impl SpeakerTracker {
    pub fn new() -> Self {
        Self {
            profiles: Vec::new(),
            high_threshold: 0.65,
            low_threshold: 0.45,
            max_speakers: 10,
            next_id: 0,
        }
    }

    /// Identify the speaker for a new (L2-normalised) embedding.
    /// Returns a label like `"SPEAKER_00"`.
    pub fn identify(&mut self, embedding: &[f32]) -> String {
        if self.profiles.is_empty() {
            return self.new_speaker(embedding);
        }

        // Find best matching profile
        let (best_idx, best_sim) = self.profiles.iter().enumerate()
            .map(|(i, p)| (i, cosine_sim(embedding, &p.embedding)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();

        if best_sim >= self.high_threshold {
            // Confident match — update EMA
            let profile = &mut self.profiles[best_idx];
            ema_update(&mut profile.embedding, embedding, 0.15);
            profile.count += 1;
            profile.label.clone()
        } else if best_sim < self.low_threshold && self.profiles.len() < self.max_speakers {
            // Confident mismatch + capacity available → new speaker
            self.new_speaker(embedding)
        } else {
            // Uncertain or at capacity — assign best match without EMA update
            self.profiles[best_idx].label.clone()
        }
    }

    fn new_speaker(&mut self, embedding: &[f32]) -> String {
        let label = format!("SPEAKER_{:02}", self.next_id);
        self.next_id += 1;
        self.profiles.push(SpeakerProfile {
            label: label.clone(),
            embedding: embedding.to_vec(),
            count: 1,
        });
        label
    }
}

/// Cosine similarity of two L2-normalised vectors (dot product).
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// EMA: embedding = (1 - alpha) * embedding + alpha * new, then re-normalise.
fn ema_update(embedding: &mut Vec<f32>, new: &[f32], alpha: f32) {
    let one_minus = 1.0 - alpha;
    for (e, n) in embedding.iter_mut().zip(new.iter()) {
        *e = one_minus * *e + alpha * n;
    }
    // Re-normalise
    let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        embedding.iter_mut().for_each(|x| *x /= norm);
    }
}
