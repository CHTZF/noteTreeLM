import { useState, useRef, useCallback, useEffect } from 'react'
import { createTranscribeSession } from '../lib/transcribeWs'
import type { TranscribeSession } from '../lib/transcribeWs'
import { useDebugStore } from '../stores/debugStore'
import { toast } from '../components/common/Toast'

export type VoiceState = 'idle' | 'recording' | 'transcribing' | 'done' | 'error'

// ─── Sample rates ─────────────────────────────────────────────────────────────
// AudioContext at 48 kHz when RNNoise is on (RNNoise is trained at 48 kHz).
// AudioContext at 16 kHz when RNNoise is off (no downsampling needed).
// The worklet always outputs 16 kHz PCM sent to server via WS.
const RNNOISE_SAMPLE_RATE = 48000
const WHISPER_SAMPLE_RATE = 16000

// ─── RNNoise module singleton (main-thread processing) ────────────────────────
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let rnnoiseModCache: any = null
// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function loadRNNoiseModule(): Promise<any> {
  if (rnnoiseModCache) return rnnoiseModCache
  const { Rnnoise } = await import('@shiguredo/rnnoise-wasm')
  rnnoiseModCache = await Rnnoise.load()
  return rnnoiseModCache
}

// ─── Client-side VAD — isSpeaking UI only ────────────────────────────────────
// Server performs actual VAD and segmentation.
const RMS_THRESHOLD       = 0.015
const SILENCE_DURATION_MS = 400

// ─── Main-thread RNNoise parameters ───────────────────────────────────────────
const DS_RATIO = 3  // 48 kHz → 16 kHz downsampling ratio

// ─── Typewriter parameters ────────────────────────────────────────────────────
const TYPEWRITER_INTERVAL_MS = 30

// ─── Segment separator (CJK languages skip space) ────────────────────────────
const CJK_LANGUAGES = new Set(['zh', 'zh-TW', 'zh-CN', 'ja', 'ko', 'yue', 'cantonese'])

function isCJKChar(ch: string): boolean {
  const cp = ch.codePointAt(0) ?? 0
  return (
    (cp >= 0x4E00 && cp <= 0x9FFF) ||
    (cp >= 0x3040 && cp <= 0x30FF) ||
    (cp >= 0xAC00 && cp <= 0xD7AF) ||
    (cp >= 0xFF00 && cp <= 0xFFEF)
  )
}

function segSep(lang: string, prevText: string, newText: string): string {
  if (!prevText) return ''
  if (CJK_LANGUAGES.has(lang)) return ''
  if (lang === 'auto') {
    if (isCJKChar(prevText.slice(-1)) || isCJKChar(newText[0] ?? '')) return ''
  }
  return ' '
}

function normalizeTranscript(text: string, lang: string): string {
  const nlSep = CJK_LANGUAGES.has(lang) ? '' : ' '
  return text.replace(/[\n\r]+/g, nlSep).trim()
}

// Vite emits this file as a static asset and resolves the URL at build time
const WORKLET_URL = new URL('../worklets/voice-processor.js', import.meta.url).href

interface UseVoiceRecorderReturn {
  state: VoiceState
  errorMsg: string
  segmentsDone: number
  isSpeaking: boolean
  toggle: () => void
}

export function useVoiceRecorder(
  onTranscript: (text: string) => void,
  _onPreview?: (text: string | null) => void,
  noiseSuppressionEnabled = true,
  _previewIntervalMs = 5000,
  whisperLanguage = 'auto',
): UseVoiceRecorderReturn {
  const [state, setState] = useState<VoiceState>('idle')
  const [errorMsg, setErrorMsg] = useState('')
  const [segmentsDone, setSegmentsDone] = useState(0)
  const [isSpeaking, setIsSpeaking] = useState(false)

  // Audio pipeline refs
  const streamRef       = useRef<MediaStream | null>(null)
  const audioCtxRef     = useRef<AudioContext | null>(null)
  const workletRef      = useRef<AudioWorkletNode | null>(null)
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const rnnoiseStateRef = useRef<any>(null)

  // WebSocket transcription session
  const transcribeSessionRef  = useRef<TranscribeSession | null>(null)
  // Resolves when server sends whisper:flush_done
  const flushDoneResolveRef   = useRef<(() => void) | null>(null)

  // Settings refs (always up-to-date inside async closures)
  const noiseSuppressionRef = useRef(noiseSuppressionEnabled)
  noiseSuppressionRef.current = noiseSuppressionEnabled
  const whisperLanguageRef = useRef(whisperLanguage)
  whisperLanguageRef.current = whisperLanguage

  // Client-side isSpeaking VAD state
  const speechActiveRef       = useRef(false)
  const speechEverDetectedRef = useRef(false)
  const silenceSamplesRef     = useRef(0)
  const silenceWarnTimerRef   = useRef<ReturnType<typeof setTimeout> | null>(null)
  const autoStopTimerRef      = useRef<ReturnType<typeof setTimeout> | null>(null)

  // stopRecording ref for timer callbacks (avoids stale closure)
  const stopRecordingRef = useRef<() => Promise<void>>(async () => {})

  // Stable callback ref
  const onTranscriptRef = useRef(onTranscript)
  onTranscriptRef.current = onTranscript

  // ─── Typewriter ──────────────────────────────────────────────────────────────
  const typewriterBufferRef = useRef<string>('')
  const typewriterTimerRef  = useRef<ReturnType<typeof setTimeout> | null>(null)
  const lastEnqueuedTextRef = useRef<string>('')

  const tickTypewriter = useCallback(() => {
    if (typewriterBufferRef.current.length === 0) {
      typewriterTimerRef.current = null
      return
    }
    const cp   = typewriterBufferRef.current.codePointAt(0)!
    const char = String.fromCodePoint(cp)
    typewriterBufferRef.current = typewriterBufferRef.current.slice(char.length)
    onTranscriptRef.current(char)
    typewriterTimerRef.current = setTimeout(tickTypewriter, TYPEWRITER_INTERVAL_MS)
  }, [])

  const enqueueTypewriter = useCallback((text: string) => {
    const sep = segSep(whisperLanguageRef.current, lastEnqueuedTextRef.current, text)
    lastEnqueuedTextRef.current = text
    typewriterBufferRef.current += sep + text
    if (typewriterTimerRef.current === null) {
      typewriterTimerRef.current = setTimeout(tickTypewriter, TYPEWRITER_INTERVAL_MS)
    }
  }, [tickTypewriter])

  const resetTypewriter = useCallback(() => {
    if (typewriterTimerRef.current !== null) {
      clearTimeout(typewriterTimerRef.current)
      typewriterTimerRef.current = null
    }
    typewriterBufferRef.current = ''
    lastEnqueuedTextRef.current = ''
  }, [])

  // ─── Eager RNNoise WASM pre-load ─────────────────────────────────────────────
  useEffect(() => {
    loadRNNoiseModule().catch(() => {})
  }, [])

  // ─── Cleanup on unmount ───────────────────────────────────────────────────────
  useEffect(() => {
    return () => {
      if (typewriterTimerRef.current !== null)  clearTimeout(typewriterTimerRef.current)
      if (silenceWarnTimerRef.current !== null) clearTimeout(silenceWarnTimerRef.current)
      if (autoStopTimerRef.current !== null)    clearTimeout(autoStopTimerRef.current)
      transcribeSessionRef.current?.close()
    }
  }, [])

  const log  = (msg: string) => useDebugStore.getState().addLog('voice',   'info',  msg)
  const warn = (msg: string) => useDebugStore.getState().addLog('voice',   'warn',  msg)
  const err  = (msg: string) => useDebugStore.getState().addLog('voice',   'error', msg)

  // ─── startRecording ──────────────────────────────────────────────────────────
  const startRecording = useCallback(async () => {
    setErrorMsg('')
    setSegmentsDone(0)
    speechActiveRef.current       = false
    speechEverDetectedRef.current = false
    silenceSamplesRef.current     = 0
    resetTypewriter()
    if (silenceWarnTimerRef.current !== null) { clearTimeout(silenceWarnTimerRef.current); silenceWarnTimerRef.current = null }
    if (autoStopTimerRef.current !== null)    { clearTimeout(autoStopTimerRef.current);    autoStopTimerRef.current = null }

    rnnoiseStateRef.current?.destroy()
    rnnoiseStateRef.current = null

    const useRNNoise = noiseSuppressionRef.current
    log(`▶ 開始錄音（AudioWorklet，雜訊抑制: ${useRNNoise ? 'RNNoise@主執行緒' : '瀏覽器內建'}）`)

    try {
      // ── 裝置檢查 ────────────────────────────────────────────────────────────
      const devices = await navigator.mediaDevices.enumerateDevices()
      if (!devices.some((d) => d.kind === 'audioinput')) {
        throw new Error('未找到麥克風裝置，請確認裝置已連接')
      }

      // ── 建立 WS 轉錄 session ─────────────────────────────────────────────────
      transcribeSessionRef.current?.close()
      const session = await createTranscribeSession(
        whisperLanguageRef.current,
        (text, index) => {
          const normalized = normalizeTranscript(text, whisperLanguageRef.current)
          if (!normalized) return
          log(`✓ 片段 #${index} 辨識完成：「${normalized.slice(0, 60)}${normalized.length > 60 ? '…' : ''}」`)
          setSegmentsDone((n) => n + 1)
          enqueueTypewriter(normalized)
        },
        () => {
          // whisper:flush_done — resolve stop() promise
          flushDoneResolveRef.current?.()
          flushDoneResolveRef.current = null
        },
        (msg) => {
          err(`轉錄錯誤：${msg}`)
        },
      )
      transcribeSessionRef.current = session
      log('✓ 轉錄 WebSocket 連線成功')

      // ── AudioContext ─────────────────────────────────────────────────────────
      const contextSampleRate = useRNNoise ? RNNOISE_SAMPLE_RATE : WHISPER_SAMPLE_RATE
      const ctx = new AudioContext({ sampleRate: contextSampleRate })
      await ctx.audioWorklet.addModule(WORKLET_URL)
      log('✓ AudioWorklet 模組載入完成')
      if (ctx.state === 'suspended') await ctx.resume()

      const worklet    = new AudioWorkletNode(ctx, 'voice-processor')
      const silentGain = ctx.createGain()
      silentGain.gain.value = 0
      worklet.connect(silentGain)
      silentGain.connect(ctx.destination)

      // ── 靜音計時器門檻 ───────────────────────────────────────────────────────
      const silenceSamplesThreshold = Math.floor(SILENCE_DURATION_MS / 1000 * WHISPER_SAMPLE_RATE)

      // ── 主執行緒音頻處理器 ───────────────────────────────────────────────────
      // Receives frames from worklet, does RNNoise + downsampling, sends PCM16 via WS
      const handleAudio = (e: MessageEvent<{ samples: Float32Array }>) => {
        const frame = e.data.samples
        let pcm16: ArrayLike<number>
        let rms: number

        if (contextSampleRate > 16000) {
          // 48 kHz: scale → RNNoise → 3:1 downsample → 16 kHz
          for (let i = 0; i < frame.length; i++) frame[i] *= 32768
          rnnoiseStateRef.current?.processFrame(frame)
          const downCount = frame.length / DS_RATIO
          const out = new Float32Array(downCount)
          let sumSq = 0
          for (let i = 0, j = 0; i < frame.length; i += DS_RATIO, j++) {
            const s = frame[i] / 32768
            out[j] = s
            sumSq += s * s
          }
          pcm16 = out
          rms   = Math.sqrt(sumSq / downCount)
        } else {
          // 16 kHz passthrough
          let sumSq = 0
          for (let i = 0; i < frame.length; i++) sumSq += frame[i] * frame[i]
          pcm16 = frame
          rms   = Math.sqrt(sumSq / frame.length)
        }

        // ── Convert float32 → Int16 and send binary frame via WS ──────────────
        const int16 = new Int16Array(pcm16.length)
        for (let i = 0; i < pcm16.length; i++) {
          const s    = Math.max(-1, Math.min(1, (pcm16 as Float32Array)[i]))
          int16[i]   = s < 0 ? s * 0x8000 : s * 0x7FFF
        }
        transcribeSessionRef.current?.sendPcm(int16.buffer)

        // ── Client-side isSpeaking VAD (UI only) ──────────────────────────────
        const speaking = rms > RMS_THRESHOLD
        if (speaking) {
          silenceSamplesRef.current = 0
          if (!speechActiveRef.current) {
            speechActiveRef.current = true
            setIsSpeaking(true)
          }
          if (!speechEverDetectedRef.current) {
            speechEverDetectedRef.current = true
            if (silenceWarnTimerRef.current !== null) {
              clearTimeout(silenceWarnTimerRef.current)
              silenceWarnTimerRef.current = null
            }
          }
          if (autoStopTimerRef.current !== null) clearTimeout(autoStopTimerRef.current)
          autoStopTimerRef.current = setTimeout(() => {
            autoStopTimerRef.current = null
            warn('連續靜音超過 30 分鐘，自動停止錄音')
            toast.warning('長時間未偵測到聲音，已自動關閉錄音')
            void stopRecordingRef.current()
          }, 30 * 60 * 1000)
        } else {
          silenceSamplesRef.current += pcm16.length
          if (speechActiveRef.current && silenceSamplesRef.current >= silenceSamplesThreshold) {
            speechActiveRef.current   = false
            silenceSamplesRef.current = 0
            setIsSpeaking(false)
          }
        }
      }

      worklet.port.onmessage = (e: MessageEvent<{ type?: string }>) => {
        if (e.data.type === 'ready') {
          worklet.port.onmessage = handleAudio
        }
      }
      worklet.port.postMessage({ type: 'init' })

      // ── Non-blocking RNNoise WASM load ───────────────────────────────────────
      if (useRNNoise) {
        ;(async () => {
          try {
            const mod = await loadRNNoiseModule()
            if (audioCtxRef.current) {
              rnnoiseStateRef.current = mod.createDenoiseState()
              log('✓ RNNoise WASM 在主執行緒載入完成')
            }
          } catch (e) {
            warn(`RNNoise 主執行緒載入失敗：${e}，降級為僅降取樣`)
          }
        })()
      }

      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: !useRNNoise },
      })
      log('✓ getUserMedia 成功')

      ctx.createMediaStreamSource(stream).connect(worklet)
      streamRef.current   = stream
      audioCtxRef.current = ctx
      workletRef.current  = worklet

      // 30s silence warning
      silenceWarnTimerRef.current = setTimeout(() => {
        silenceWarnTimerRef.current = null
        if (!speechEverDetectedRef.current) {
          warn('持續靜音 30 秒，可能麥克風未啟用')
          toast.warning('未偵測到聲音，請確認麥克風裝置是否已啟用')
        }
      }, 30_000)

      // 30min auto-stop
      autoStopTimerRef.current = setTimeout(() => {
        autoStopTimerRef.current = null
        warn('連續靜音超過 30 分鐘，自動停止錄音')
        toast.warning('長時間未偵測到聲音，已自動關閉錄音')
        void stopRecordingRef.current()
      }, 30 * 60 * 1000)

      setState('recording')
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      err(`錄音啟動失敗：${msg}`)
      toast.error(msg)
      setErrorMsg(msg)
      setState('error')
      setTimeout(() => setState('idle'), 4000)
    }
  }, [enqueueTypewriter, resetTypewriter])

  // ─── stopRecording ───────────────────────────────────────────────────────────
  const stopRecording = useCallback(async () => {
    const stream  = streamRef.current
    const ctx     = audioCtxRef.current
    const worklet = workletRef.current

    if (!stream || !ctx || !worklet) {
      warn('stopRecording 呼叫時無有效串流，略過')
      return
    }

    // Stop audio pipeline
    worklet.disconnect()
    worklet.port.close()
    stream.getTracks().forEach((t) => t.stop())
    await ctx.close()
    rnnoiseStateRef.current?.destroy()
    rnnoiseStateRef.current = null

    streamRef.current   = null
    audioCtxRef.current = null
    workletRef.current  = null

    // Clear timers
    if (silenceWarnTimerRef.current !== null) { clearTimeout(silenceWarnTimerRef.current); silenceWarnTimerRef.current = null }
    if (autoStopTimerRef.current !== null)    { clearTimeout(autoStopTimerRef.current);    autoStopTimerRef.current = null }
    setIsSpeaking(false)
    speechActiveRef.current       = false
    speechEverDetectedRef.current = false
    silenceSamplesRef.current     = 0

    log('⏹ 停止錄音，等待 server flush…')
    setState('transcribing')

    // Tell server to flush remaining buffer; wait for flush_done
    const session = transcribeSessionRef.current
    if (session) {
      await new Promise<void>((resolve) => {
        flushDoneResolveRef.current = resolve
        session.stop()
        // Fallback timeout: if server never replies, resolve after 15s
        setTimeout(resolve, 15_000)
      })
      session.close()
      transcribeSessionRef.current = null
    }

    // Wait for typewriter to finish
    while (typewriterBufferRef.current.length > 0) {
      await new Promise<void>((r) => setTimeout(r, 50))
    }

    setState('done')
    setTimeout(() => setState('idle'), 2000)
  }, [])

  // ─── Toggle ──────────────────────────────────────────────────────────────────
  stopRecordingRef.current = stopRecording

  const toggle = useCallback(() => {
    if (state === 'idle' || state === 'done' || state === 'error') {
      startRecording()
    } else if (state === 'recording') {
      stopRecording()
    }
  }, [state, startRecording, stopRecording])

  return { state, errorMsg, segmentsDone, isSpeaking, toggle }
}
