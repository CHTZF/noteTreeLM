import { useState, useRef, useCallback, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useDebugStore } from '../stores/debugStore'
import { toast } from '../components/common/Toast'

export type VoiceState = 'idle' | 'recording' | 'transcribing' | 'done' | 'error'

// ─── VAD parameters ───────────────────────────────────────────────────────────
const TARGET_SAMPLE_RATE  = 16000   // Whisper 原生取樣率；請求 AudioContext 在此率輸出
                                    // AudioWorklet 每次 process() 固定 128 樣本
                                    // 16000Hz / 128 ≈ 8ms per callback
const RMS_THRESHOLD       = 0.01    // 均方根能量門檻：低於此值視為靜音（0–1 float scale）
const SILENCE_DURATION_MS = 400     // 靜音持續多久後觸發沖出（ms）
const MIN_SEGMENT_SEC     = 0.3     // 最短有效片段（秒）

// Vite emits this file as a static asset and resolves the URL at build time
const WORKLET_URL = new URL('../worklets/voice-processor.js', import.meta.url).href

interface UseVoiceRecorderReturn {
  state: VoiceState
  errorMsg: string
  segmentsDone: number
  toggle: () => void
}

export function useVoiceRecorder(
  onTranscript: (text: string) => void,
): UseVoiceRecorderReturn {
  const [state, setState] = useState<VoiceState>('idle')
  const [errorMsg, setErrorMsg] = useState('')
  const [segmentsDone, setSegmentsDone] = useState(0)

  const streamRef    = useRef<MediaStream | null>(null)
  const audioCtxRef  = useRef<AudioContext | null>(null)
  const workletRef   = useRef<AudioWorkletNode | null>(null)

  const samplesRef       = useRef<number[]>([])
  const sampleRateRef    = useRef(TARGET_SAMPLE_RATE)
  const chunkStartRef    = useRef(0)

  // VAD state — maintained inside port.onmessage, no setInterval needed
  const speechActiveRef   = useRef(false)
  const silenceSamplesRef = useRef(0)    // accumulated silent samples since last speech

  // Processing queue
  const queueRef      = useRef<number[][]>([])
  const processingRef = useRef(false)

  // Stable ref for callback
  const onTranscriptRef = useRef(onTranscript)
  onTranscriptRef.current = onTranscript

  const log  = (msg: string) => useDebugStore.getState().addLog('voice',   'info',  msg)
  const warn = (msg: string) => useDebugStore.getState().addLog('voice',   'warn',  msg)
  const err  = (msg: string) => useDebugStore.getState().addLog('voice',   'error', msg)
  const wlog = (msg: string) => useDebugStore.getState().addLog('whisper', 'info',  msg)

  // 監聽 whisper-server 啟動進度，給使用者即時回饋
  const loadingToastIdRef = useRef<number | null>(null)

  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | null = null
    listen<string>('whisper:stderr', (event) => {
      const line = event.payload
      wlog(line)

      if (line.startsWith('[server:error]')) {
        // 設定錯誤（模型檔案不存在等）→ 永久顯示 error toast 直到使用者點擊關閉
        const msg = line.replace('[server:error] ', '')
        toast.error(msg, { duration: 0 })
      } else if (line.includes('等待模型載入') || line.includes('模型載入中')) {
        // 持續顯示直到手動 dismiss（duration: 0 = 不自動消失）
        if (loadingToastIdRef.current === null) {
          loadingToastIdRef.current = toast.info('whisper-server 載入模型中，請稍候…', { duration: 0 })
        }
      } else if (line.includes('就緒')) {
        if (loadingToastIdRef.current !== null) {
          toast.dismiss(loadingToastIdRef.current)
          loadingToastIdRef.current = null
        }
        toast.info('whisper-server 已就緒')
      }
    }).then((fn) => {
      if (cancelled) fn() // cleanup 已執行，立即取消訂閱
      else unlisten = fn
    })

    return () => {
      cancelled = true
      unlisten?.()
    }
  }, [])

  // ─── Queue ──────────────────────────────────────────────────────────────────

  const drainQueue = useCallback(async () => {
    if (processingRef.current) return
    processingRef.current = true
    try {
      while (queueRef.current.length > 0) {
        const chunk = queueRef.current.shift()!
        const durationSec = (chunk.length / sampleRateRef.current).toFixed(1)
        log(`→ 處理片段（${durationSec}s，${chunk.length} 樣本）`)
        try {
          const result = await invoke<{ text: string }>('transcribe_audio', {
            pcmData: chunk,
            sampleRate: sampleRateRef.current,
          })
          const text = result.text.trim()
          if (text) {
            log(`✓ 片段辨識完成：「${text.slice(0, 60)}${text.length > 60 ? '…' : ''}」`)
            setSegmentsDone((n) => n + 1)
            onTranscriptRef.current(text)
          } else {
            log('  片段辨識結果為空，略過')
          }
        } catch (e: unknown) {
          const msg = typeof e === 'string' ? e
            : (() => { try { return JSON.stringify(e, null, 2) } catch { return String(e) } })()
          err(`片段辨識失敗：${msg}`)
        }
      }
    } finally {
      processingRef.current = false
    }
  }, [])

  const enqueueChunk = useCallback((chunk: number[]) => {
    queueRef.current.push(chunk)
    void drainQueue()
  }, [drainQueue])

  // ─── Recording ──────────────────────────────────────────────────────────────

  const startRecording = useCallback(async () => {
    setErrorMsg('')
    setSegmentsDone(0)
    queueRef.current       = []
    processingRef.current  = false
    speechActiveRef.current = false
    silenceSamplesRef.current = 0
    chunkStartRef.current  = 0

    log(`▶ 開始錄音（AudioWorklet/VAD 模式，目標 ${TARGET_SAMPLE_RATE}Hz）`)
    try {
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true },
      })
      log('✓ getUserMedia 成功')

      // 要求 AudioContext 輸出 16kHz；瀏覽器會自動 resample 麥克風原生取樣率
      const ctx = new AudioContext({ sampleRate: TARGET_SAMPLE_RATE })
      sampleRateRef.current = ctx.sampleRate
      samplesRef.current = []
      log(`AudioContext 實際 sampleRate: ${ctx.sampleRate} Hz`)

      // 載入 AudioWorklet 模組
      await ctx.audioWorklet.addModule(WORKLET_URL)
      log('✓ AudioWorklet 模組載入完成')

      const silenceSamplesThreshold = Math.floor(
        SILENCE_DURATION_MS / 1000 * sampleRateRef.current
      )
      const minSegmentSamples = Math.floor(MIN_SEGMENT_SEC * sampleRateRef.current)

      const source  = ctx.createMediaStreamSource(stream)
      const worklet = new AudioWorkletNode(ctx, 'voice-processor')

      worklet.port.onmessage = (e: MessageEvent<{ samples: Float32Array; rms: number }>) => {
        const { samples, rms } = e.data

        // 累積樣本
        for (let i = 0; i < samples.length; i++) {
          samplesRef.current.push(samples[i])
        }

        // VAD 判斷（使用 worklet 已計算好的 RMS）
        const isSpeaking = rms > RMS_THRESHOLD

        if (isSpeaking) {
          silenceSamplesRef.current = 0
          speechActiveRef.current = true
        } else {
          silenceSamplesRef.current += samples.length
          if (speechActiveRef.current && silenceSamplesRef.current >= silenceSamplesThreshold) {
            speechActiveRef.current = false
            silenceSamplesRef.current = 0

            const endIdx = samplesRef.current.length
            const chunk  = samplesRef.current.slice(chunkStartRef.current, endIdx)
            if (chunk.length >= minSegmentSamples) {
              log(`VAD: 沖出 ${(chunk.length / sampleRateRef.current).toFixed(1)}s 片段`)
              enqueueChunk([...chunk])
              chunkStartRef.current = endIdx
            }
          }
        }
      }

      // AudioWorklet 只需接收輸入，不必連接到 destination
      source.connect(worklet)

      streamRef.current   = stream
      audioCtxRef.current = ctx
      workletRef.current  = worklet

      setState('recording')
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      err(`錄音啟動失敗：${msg}`)
      setErrorMsg(msg)
      setState('error')
      setTimeout(() => setState('idle'), 4000)
    }
  }, [enqueueChunk])

  const stopRecording = useCallback(async () => {
    const stream  = streamRef.current
    const ctx     = audioCtxRef.current
    const worklet = workletRef.current

    if (!stream || !ctx || !worklet) {
      warn('stopRecording 呼叫時無有效串流，略過')
      return
    }

    worklet.disconnect()
    worklet.port.close()
    stream.getTracks().forEach((t) => t.stop())
    await ctx.close()

    streamRef.current   = null
    audioCtxRef.current = null
    workletRef.current  = null

    const totalSamples   = samplesRef.current.length
    const remaining      = samplesRef.current.slice(chunkStartRef.current)
    // 讀取 VAD 狀態後再清零：speechActiveRef = true 表示尚有未 flush 的語音
    const hasUnsentSpeech = speechActiveRef.current
    samplesRef.current = []
    chunkStartRef.current = 0
    speechActiveRef.current = false
    silenceSamplesRef.current = 0

    log(`⏹ 停止錄音，共 ${totalSamples} 樣本（${(totalSamples / sampleRateRef.current).toFixed(2)}s @ ${sampleRateRef.current}Hz）`)

    // 只在有未 flush 語音時才傳送剩餘片段
    // （若 VAD 已正常 flush，remaining 僅為靜音，無需傳送）
    if (hasUnsentSpeech && remaining.length >= sampleRateRef.current * 0.3) {
      log(`→ 剩餘 ${(remaining.length / sampleRateRef.current).toFixed(1)}s，加入佇列`)
      enqueueChunk([...remaining])
    }

    if (queueRef.current.length === 0 && !processingRef.current) {
      if (totalSamples < sampleRateRef.current * 0.3) {
        warn('錄音過短（< 0.3 秒），略過辨識')
      }
      setState('idle')
      return
    }

    setState('transcribing')

    while (queueRef.current.length > 0 || processingRef.current) {
      await new Promise<void>((r) => setTimeout(r, 50))
    }

    setState('done')
    setTimeout(() => setState('idle'), 2000)
  }, [enqueueChunk])

  // ─── Toggle ─────────────────────────────────────────────────────────────────

  const toggle = useCallback(() => {
    if (state === 'idle' || state === 'done' || state === 'error') {
      startRecording()
    } else if (state === 'recording') {
      stopRecording()
    }
  }, [state, startRecording, stopRecording])

  return { state, errorMsg, segmentsDone, toggle }
}
