/**
 * AudioWorklet processor for voice recording.
 * Runs in the audio rendering thread; sends PCM samples + RMS energy to the main thread
 * on every render quantum (128 samples at 16 kHz ≈ 8 ms per callback).
 */
class VoiceProcessor extends AudioWorkletProcessor {
  process(inputs) {
    const channel = inputs[0]?.[0]
    if (channel && channel.length > 0) {
      let sumSq = 0
      for (let i = 0; i < channel.length; i++) sumSq += channel[i] * channel[i]
      const rms = Math.sqrt(sumSq / channel.length)
      // slice() copies the Float32Array (the original is owned by the audio thread)
      this.port.postMessage({ samples: channel.slice(), rms })
    }
    return true // keep processor alive
  }
}

registerProcessor('voice-processor', VoiceProcessor)
