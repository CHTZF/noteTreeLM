use std::sync::Mutex;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Tensor;

/// Wraps the 3D-Speaker CAM++ ONNX model for speaker embedding extraction.
///
/// Input:  PCM16 samples at 16 kHz
/// Output: L2-normalised embedding vector (192 dims for CAM++)
///
/// The session is Mutex-guarded so this can be shared across connections via Arc.
pub struct EmbedModel {
    session: Mutex<Session>,
}

impl EmbedModel {
    pub fn load(model_path: &str) -> Result<Self, String> {
        let session = Session::builder()
            .map_err(|e| e.to_string())?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| e.to_string())?
            .with_intra_threads(1)
            .map_err(|e| e.to_string())?
            .commit_from_file(model_path)
            .map_err(|e| format!("無法載入 diarize 模型 {model_path}: {e}"))?;
        Ok(Self { session: Mutex::new(session) })
    }

    /// Extract a speaker embedding from a PCM16 segment.
    pub fn extract(&self, samples: &[i16]) -> Result<Vec<f32>, String> {
        // ── 1. i16 → f32 ────────────────────────────────────────────────────
        let mut samples_f32 = vec![0.0f32; samples.len()];
        knf_rs::convert_integer_to_float_audio(samples, &mut samples_f32);

        // ── 2. 80-dim mel-fbank via Kaldi fbank ─────────────────────────────
        let features = knf_rs::compute_fbank(&samples_f32)
            .map_err(|e| e.to_string())?;
        // features: ndarray::Array2<f32> [num_frames, 80]
        let num_frames = features.nrows();
        if num_frames == 0 {
            return Err("segment too short for fbank".to_string());
        }
        let num_mel = features.ncols(); // 80

        // ── 3. Flatten to Vec and build shape [1, num_frames, num_mel] ────────
        // into_iter() iterates in logical (row-major) order regardless of
        // underlying memory layout — safer than into_raw_vec_and_offset()
        // which depends on the array having zero offset and standard strides.
        let flat: Vec<f32> = features.into_iter().collect();
        let shape = [1usize, num_frames, num_mel];

        // ── 4. ONNX inference (no ndarray feature needed — use raw shape+vec) ─
        let input = Tensor::from_array((shape, flat))
            .map_err(|e| e.to_string())?;
        let mut guard = self.session
            .lock()
            .map_err(|_| "session mutex poisoned".to_string())?;
        let outputs = guard
            .run(ort::inputs!["feats" => input])
            .map_err(|e| e.to_string())?;

        // try_extract_tensor returns (&Shape, &[T])
        let (_, emb_data) = outputs["embs"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;

        // ── 5. L2 normalise ──────────────────────────────────────────────────
        Ok(l2_normalise(emb_data.to_vec()))
    }
}

// Mutex<Session> is Send + Sync as long as Session is Send, which it is in ort 2.x
unsafe impl Send for EmbedModel {}
unsafe impl Sync for EmbedModel {}

fn l2_normalise(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}
