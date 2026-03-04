/**
 * AudioWorklet processor for voice recording.
 *
 * Protocol (port messages):
 *  →  { type: 'init', rnnoiseUrl: string, enabled: boolean }
 *  ←  { type: 'ready', rnnoiseError?: string }
 *  ←  { samples: Float32Array, rms: number }   ← always 16 kHz PCM
 *
 * Two operating modes:
 *  RNNoise (enabled=true, AudioContext 48 kHz):
 *    Accumulates 480@48kHz → RNNoise → downsample 3:1 → post 160@16kHz
 *  Raw (enabled=false, AudioContext 16 kHz):
 *    Pass through 128@16kHz per render quantum
 *
 * If RNNoise import fails the processor transparently falls back to
 * raw/downsampled mode and reports the error in the ready message.
 */

const RNNOISE_FRAME = 480  // 48 kHz × 10 ms = one RNNoise frame
const DS_RATIO      = 3    // 48 kHz → 16 kHz

class VoiceProcessor extends AudioWorkletProcessor {
  constructor() {
    super()
    this._enabled = false
    this._ready   = false
    this._state   = null   // DenoiseState (lives in the audio rendering thread)
    this._buf     = []     // 48 kHz accumulation buffer

    this.port.onmessage = ({ data }) => {
      if (data.type === 'init') {
        this._enabled = !!data.enabled
        if (this._enabled && data.rnnoiseUrl) {
          this._loadRnnoise(data.rnnoiseUrl)
        } else {
          this._ready = true
          this.port.postMessage({ type: 'ready' })
        }
      }
    }
  }

  async _loadRnnoise(url) {
    try {
      // Safari / WKWebView: AudioWorkletGlobalScope does not expose WorkerGlobalScope
      // as a named global.  rnnoise.js (Emscripten) checks
      //   `typeof WorkerGlobalScope < "u"`
      // to detect a browser-worker environment.  Polyfill the symbol so the check
      // passes — any non-undefined typeof value whose first char is < 'u' works.
      if (typeof WorkerGlobalScope === 'undefined') {
        globalThis.WorkerGlobalScope = function WorkerGlobalScope() {}
      }

      const { Rnnoise } = await import(url)
      const mod   = await Rnnoise.load()
      this._state = mod.createDenoiseState()
      this._ready = true
      this.port.postMessage({ type: 'ready' })
    } catch (err) {
      // Graceful fallback: disable RNNoise, still deliver audio in downsampled form
      this._enabled = false
      this._state   = null
      this._ready   = true
      this.port.postMessage({ type: 'ready', rnnoiseError: String(err) })
    }
  }

  process(inputs) {
    if (!this._ready) return true
    const ch = inputs[0]?.[0]
    if (!ch?.length) return true

    if (this._enabled && this._state) {
      // ── RNNoise path: 48 kHz → RNNoise → 3:1 downsample → 16 kHz ──────────
      for (let i = 0; i < ch.length; i++) this._buf.push(ch[i])

      while (this._buf.length >= RNNOISE_FRAME) {
        // Scale to 16-bit range (RNNoise convention: input/output in [-32768, 32768])
        const frame = new Float32Array(RNNOISE_FRAME)
        for (let i = 0; i < RNNOISE_FRAME; i++) frame[i] = this._buf[i] * 32768
        this._buf.splice(0, RNNOISE_FRAME)

        this._state.processFrame(frame)   // in-place denoising, returns VAD prob

        const downCount = RNNOISE_FRAME / DS_RATIO   // 160 samples @ 16 kHz
        const out = new Float32Array(downCount)
        let sumSq = 0
        for (let i = 0, j = 0; i < RNNOISE_FRAME; i += DS_RATIO, j++) {
          const s = frame[i] / 32768
          out[j] = s
          sumSq  += s * s
        }
        this.port.postMessage(
          { samples: out, rms: Math.sqrt(sumSq / downCount) },
          [out.buffer],
        )
      }

    } else if (sampleRate > 16000) {
      // ── Fallback: 48 kHz context but RNNoise unavailable — downsample only ──
      for (let i = 0; i < ch.length; i++) this._buf.push(ch[i])

      while (this._buf.length >= RNNOISE_FRAME) {
        const downCount = RNNOISE_FRAME / DS_RATIO
        const out = new Float32Array(downCount)
        let sumSq = 0
        for (let i = 0, j = 0; i < RNNOISE_FRAME; i += DS_RATIO, j++) {
          const s = this._buf[i]
          out[j] = s
          sumSq  += s * s
        }
        this._buf.splice(0, RNNOISE_FRAME)
        this.port.postMessage(
          { samples: out, rms: Math.sqrt(sumSq / downCount) },
          [out.buffer],
        )
      }

    } else {
      // ── 16 kHz pass-through (RNNoise disabled, AudioContext at 16 kHz) ─────
      let sumSq = 0
      for (let i = 0; i < ch.length; i++) sumSq += ch[i] * ch[i]
      const copy = ch.slice()
      this.port.postMessage(
        { samples: copy, rms: Math.sqrt(sumSq / ch.length) },
        [copy.buffer],
      )
    }

    return true
  }
}

registerProcessor('voice-processor', VoiceProcessor)
