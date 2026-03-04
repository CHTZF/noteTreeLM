/**
 * AudioWorklet processor for voice recording.
 *
 * Raw frame relay — all audio processing (RNNoise, downsampling, RMS)
 * happens on the main thread.  Dynamic import() is prohibited in
 * AudioWorkletGlobalScope, so WASM cannot be loaded here.
 *
 * Protocol (port messages):
 *  →  { type: 'init' }
 *  ←  { type: 'ready' }
 *  ←  { samples: Float32Array }
 *     48 kHz context: 480-sample frames (one RNNoise frame @ 48 kHz)
 *     16 kHz context: 128-sample render quanta (pass-through)
 */

const RNNOISE_FRAME = 480  // 48 kHz × 10 ms — one RNNoise frame (main thread)

class VoiceProcessor extends AudioWorkletProcessor {
  constructor() {
    super()
    this._ready = false
    this._buf   = []   // 48 kHz accumulation buffer

    this.port.onmessage = ({ data }) => {
      if (data.type === 'init') {
        this._ready = true
        this.port.postMessage({ type: 'ready' })
      }
    }
  }

  process(inputs) {
    if (!this._ready) return true
    const ch = inputs[0]?.[0]
    if (!ch?.length) return true

    if (sampleRate > 16000) {
      // 48 kHz: accumulate until we have a full RNNoise frame, then relay raw
      for (let i = 0; i < ch.length; i++) this._buf.push(ch[i])
      while (this._buf.length >= RNNOISE_FRAME) {
        const frame = new Float32Array(this._buf.splice(0, RNNOISE_FRAME))
        this.port.postMessage({ samples: frame }, [frame.buffer])
      }
    } else {
      // 16 kHz: relay each render quantum as-is
      const copy = ch.slice()
      this.port.postMessage({ samples: copy }, [copy.buffer])
    }

    return true
  }
}

registerProcessor('voice-processor', VoiceProcessor)
