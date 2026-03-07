import { useState, useRef, useCallback, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useDebugStore } from '../stores/debugStore'
import { toast } from '../components/common/Toast'
export type VoiceState = 'idle' | 'recording' | 'transcribing' | 'done' | 'error'

// ─── Sample rates ─────────────────────────────────────────────────────────────
// AudioContext at 48 kHz when RNNoise is on (RNNoise is trained at 48 kHz).
// AudioContext at 16 kHz when RNNoise is off (no downsampling needed).
// The worklet always outputs 16 kHz PCM; samplesRef always holds 16 kHz data.
const RNNOISE_SAMPLE_RATE = 48000  // AudioContext sample rate when RNNoise enabled
const WHISPER_SAMPLE_RATE = 16000  // samplesRef / whisper target sample rate

// ─── RNNoise module singleton (main-thread processing) ────────────────────────
// Dynamic import() is prohibited in AudioWorkletGlobalScope, so RNNoise runs on
// the main thread instead.  The WASM module is cached after first load; only
// DenoiseState objects are created/destroyed per recording session.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
let rnnoiseModCache: any = null
// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function loadRNNoiseModule(): Promise<any> {
  if (rnnoiseModCache) return rnnoiseModCache
  const { Rnnoise } = await import('@shiguredo/rnnoise-wasm')
  rnnoiseModCache = await Rnnoise.load()
  return rnnoiseModCache
}

// ─── VAD parameters ───────────────────────────────────────────────────────────
// samplesRef 儲存 16kHz 降取樣後的音頻，供 whisper 使用。
// 所有基於樣本數的計算（門檻、片段長度）皆以 16kHz 為準。
const RMS_THRESHOLD       = 0.015  // 均方根能量門檻：低於此值視為靜音（0–1 float scale）
                                    // 0.015 比原本 0.01 更嚴格，減少環境噪音誤觸發
const SILENCE_DURATION_MS = 400    // 靜音持續多久後觸發沖出（ms）
const MIN_SEGMENT_SEC     = 0.3    // 最短有效片段（秒）
// chunk 整體能量門檻：VAD flush 前先確認整段平均能量，低於此值視為環境噪音不送 whisper
// 真實語音即使偏小聲 RMS 仍通常 > 0.008；環境背景音通常在 0.002–0.008 之間
// 0.008（從 0.005 調高）：更嚴格過濾環境噪音，減少低能量段觸發 whisper 幻覺
const MIN_CHUNK_RMS       = 0.008

// ─── Whisper 幻覺過濾 ──────────────────────────────────────────────────────────
// whisper.cpp 對靜音或雜訊音頻會輸出固定字串（訓練資料噪音）。
// 已知幻覺字串以子字串匹配過濾；另外偵測明顯重複句型。
const HALLUCINATION_PATTERNS = [
  '请不吝点赞',
  '订阅 转发',
  '打赏支持',
  '明镜与点点',
  '字幕由',
  '敬请订阅',
  '感谢观看',
  '请关注',
]

function isWhisperHallucination(text: string): boolean {
  if (HALLUCINATION_PATTERNS.some((p) => text.includes(p))) return true
  // 偵測重複幻覺：把 text 以空白切分後，前半與後半完全相同
  const words = text.trim().split(/\s+/)
  if (words.length >= 6) {
    const half   = Math.floor(words.length / 2)
    const first  = words.slice(0, half).join(' ')
    const second = words.slice(half, half * 2).join(' ')
    if (first === second) return true
  }
  return false
}

/** 計算 PCM 樣本陣列的 RMS 能量 */
function chunkRms(samples: number[]): number {
  if (samples.length === 0) return 0
  let sumSq = 0
  for (let i = 0; i < samples.length; i++) sumSq += samples[i] * samples[i]
  return Math.sqrt(sumSq / samples.length)
}

// ─── Typewriter parameters ────────────────────────────────────────────────────
// 轉錄結果不立即輸出，而是逐字顯示，模擬串流感
// CJK 字元平均約 3-4 字/秒，30ms/字視覺上順暢且不會拖慢輸出
const TYPEWRITER_INTERVAL_MS = 30

// ─── Preview parameters ───────────────────────────────────────────────────────
// 每隔 voice_preview_interval 秒把尚未 VAD-flush 的音頻送 whisper 預覽（best-effort，可能不完整）
// VAD 沖出後立刻令牌失效並清除預覽，由正式轉錄結果取代
const MIN_PREVIEW_SEC           = 1.5   // 至少需要這麼長的音頻才觸發預覽
// 已處理音頻（rawPreviewChunkStartRef 之前）超過此樣本數時壓縮陣列，釋放記憶體
const COMPACT_THRESHOLD_SAMPLES = 5 * WHISPER_SAMPLE_RATE  // 5 秒

// ─── Main-thread RNNoise parameters ───────────────────────────────────────────
const DS_RATIO = 3  // 48 kHz → 16 kHz downsampling ratio

// ─── Preview separator（CJK 語言不加空格）────────────────────────────────────
// whisper_language 設定為 CJK 語系時段落拼接不插入空格；
// 設為 'auto' 時依文字內容自動偵測，兼容混合語言。
const CJK_LANGUAGES = new Set(['zh', 'zh-TW', 'zh-CN', 'ja', 'ko', 'yue', 'cantonese'])

function isCJKChar(ch: string): boolean {
  const cp = ch.codePointAt(0) ?? 0
  return (
    (cp >= 0x4E00 && cp <= 0x9FFF) ||  // CJK Unified Ideographs
    (cp >= 0x3040 && cp <= 0x30FF) ||  // Hiragana + Katakana
    (cp >= 0xAC00 && cp <= 0xD7AF) ||  // Hangul
    (cp >= 0xFF00 && cp <= 0xFFEF)     // Fullwidth forms
  )
}

/** whisper.cpp 輸出的文字內部可能含有 \n（句子邊界標記），需先移除。
 *  CJK 語言：\n → ''（中日韓字元間不需空格）
 *  其他語言：\n → ' '（保留詞間空格）
 *  'auto'：保守使用 ' '，避免誤刪有意義的字詞間隔 */
function normalizeTranscript(text: string, lang: string): string {
  const nlSep = CJK_LANGUAGES.has(lang) ? '' : ' '
  return text.replace(/[\n\r]+/g, nlSep).trim()
}

/** 根據語言設定決定 preview 段落間分隔符：CJK 語系 → ''，其餘 → ' '
 *  - 明確設定語言：直接查 CJK_LANGUAGES，不做文字偵測
 *  - 'auto'：依前段末尾與新段開頭字元自動判斷 */
function previewSep(lang: string, prevText: string, newText: string): string {
  if (!prevText) return ''
  if (CJK_LANGUAGES.has(lang)) return ''
  if (lang === 'auto') {
    if (isCJKChar(prevText.slice(-1)) || isCJKChar(newText[0] ?? '')) return ''
  }
  return ' '
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
  onPreview?: (text: string | null) => void,
  noiseSuppressionEnabled = true,
  previewIntervalMs = 5000,
  whisperLanguage = 'auto',
): UseVoiceRecorderReturn {
  const [state, setState] = useState<VoiceState>('idle')
  const [errorMsg, setErrorMsg] = useState('')
  const [segmentsDone, setSegmentsDone] = useState(0)
  const [isSpeaking, setIsSpeaking] = useState(false)

  const streamRef    = useRef<MediaStream | null>(null)
  const audioCtxRef  = useRef<AudioContext | null>(null)
  const workletRef   = useRef<AudioWorkletNode | null>(null)
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const rnnoiseStateRef = useRef<any>(null)  // DenoiseState — created per session, destroyed on stop

  // Chain B: 16kHz raw AudioContext — 不跑 RNNoise，專供 preview 計時器使用
  // 與 Chain A（RNNoise）並行，不等 WASM 載入即可開始累積音頻
  const rawCtxRef              = useRef<AudioContext | null>(null)
  const rawWorkletRef          = useRef<AudioWorkletNode | null>(null)
  const rawPreviewSamplesRef   = useRef<number[]>([])
  const rawPreviewChunkStartRef = useRef(0)
  const previewTextAccumRef    = useRef<string>('')  // 各段 preview 文字累積，VAD flush 時清空

  const samplesRef       = useRef<number[]>([])
  const sampleRateRef    = useRef(WHISPER_SAMPLE_RATE)  // 永遠是 16kHz（samplesRef 的實際取樣率）
  const chunkStartRef    = useRef(0)

  // 雜訊抑制設定（ref 讓 startRecording 閉包永遠讀到最新值）
  const noiseSuppressionRef = useRef(noiseSuppressionEnabled)
  noiseSuppressionRef.current = noiseSuppressionEnabled

  // 預覽間隔（ref 讓 startRecording 閉包永遠讀到最新值）
  const previewIntervalMsRef = useRef(previewIntervalMs)
  previewIntervalMsRef.current = previewIntervalMs

  // 語言設定（ref 讓 startRecording 閉包永遠讀到最新值）
  const whisperLanguageRef = useRef(whisperLanguage)
  whisperLanguageRef.current = whisperLanguage

  // VAD state — maintained inside port.onmessage, no setInterval needed
  const speechActiveRef       = useRef(false)
  const speechEverDetectedRef = useRef(false)  // 本次錄音是否曾偵測到聲音
  const silenceSamplesRef     = useRef(0)       // accumulated silent samples since last speech
  const silenceWarnTimerRef   = useRef<ReturnType<typeof setTimeout> | null>(null)  // 30s 無聲提示
  const autoStopTimerRef      = useRef<ReturnType<typeof setTimeout> | null>(null)  // 30min 無聲自動停止

  // 穩定的 stopRecording ref，供計時器回呼使用（避免 stale closure）
  const stopRecordingRef = useRef<() => Promise<void>>(async () => {})

  // Processing queue
  const queueRef      = useRef<number[][]>([])
  const processingRef = useRef(false)

  // Stable ref for callbacks
  const onTranscriptRef = useRef(onTranscript)
  onTranscriptRef.current = onTranscript
  const onPreviewRef = useRef(onPreview)
  onPreviewRef.current = onPreview

  // Preview interval state
  const previewTimerRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const previewBusyRef  = useRef(false)
  // 每次 VAD flush 時遞增；預覽回呼以此判斷結果是否已過期
  const previewGenRef   = useRef(0)

  // ─── Typewriter buffer ──────────────────────────────────────────────────────
  // 儲存待逐字輸出的文字；轉錄結果 enqueue 後由 tickTypewriter 逐字 drain
  const typewriterBufferRef   = useRef<string>('')
  const typewriterTimerRef    = useRef<ReturnType<typeof setTimeout> | null>(null)
  // 記錄上一個 enqueue 的 segment 原始文字，供 CJK 分隔符偵測使用
  const lastEnqueuedTextRef   = useRef<string>('')

  // 每隔 TYPEWRITER_INTERVAL_MS 輸出一個字元
  const tickTypewriter = useCallback(() => {
    if (typewriterBufferRef.current.length === 0) {
      typewriterTimerRef.current = null
      return
    }
    // 取第一個 Unicode codepoint（正確處理 emoji 與 surrogate pair）
    const cp   = typewriterBufferRef.current.codePointAt(0)!
    const char = String.fromCodePoint(cp)
    typewriterBufferRef.current = typewriterBufferRef.current.slice(char.length)
    onTranscriptRef.current(char)
    typewriterTimerRef.current = setTimeout(tickTypewriter, TYPEWRITER_INTERVAL_MS)
  }, [])

  // 將文字加入 typewriter queue；若 ticker 尚未運行則啟動
  // 段落分隔符依語言設定決定：CJK → ''，其餘 → ' '
  const enqueueTypewriter = useCallback((text: string) => {
    const sep = previewSep(whisperLanguageRef.current, lastEnqueuedTextRef.current, text)
    lastEnqueuedTextRef.current = text
    typewriterBufferRef.current += sep + text
    if (typewriterTimerRef.current === null) {
      typewriterTimerRef.current = setTimeout(tickTypewriter, TYPEWRITER_INTERVAL_MS)
    }
  }, [tickTypewriter])

  // 清除 typewriter（新錄音開始時重置）
  const resetTypewriter = useCallback(() => {
    if (typewriterTimerRef.current !== null) {
      clearTimeout(typewriterTimerRef.current)
      typewriterTimerRef.current = null
    }
    typewriterBufferRef.current = ''
    lastEnqueuedTextRef.current = ''
  }, [])

  // ─── Eager RNNoise WASM pre-load ──────────────────────────────────────────────
  // 元件掛載時就開始載入 RNNoise WASM 模組並快取，
  // 讓使用者點錄音時 DenoiseState 幾乎可以立即建立（避免前幾秒降到純降取樣）。
  useEffect(() => {
    loadRNNoiseModule().catch(() => {})
  }, [])

  // 元件卸載時清除所有計時器與 Chain B，避免 memory leak
  useEffect(() => {
    return () => {
      if (typewriterTimerRef.current !== null) clearTimeout(typewriterTimerRef.current)
      if (silenceWarnTimerRef.current !== null) clearTimeout(silenceWarnTimerRef.current)
      if (autoStopTimerRef.current !== null)    clearTimeout(autoStopTimerRef.current)
      if (previewTimerRef.current !== null)     clearInterval(previewTimerRef.current)
      rawWorkletRef.current?.disconnect()
      rawWorkletRef.current?.port.close()
      void rawCtxRef.current?.close()
    }
  }, [])

  const log  = (msg: string) => useDebugStore.getState().addLog('voice',   'info',  msg)
  const warn = (msg: string) => useDebugStore.getState().addLog('voice',   'warn',  msg)
  const err  = (msg: string) => useDebugStore.getState().addLog('voice',   'error', msg)
  const wlog = (msg: string) => useDebugStore.getState().addLog('whisper', 'info',  msg)

  // whisper:stderr → debug log only（toast 通知由 App.tsx 統一處理，避免多實例重複顯示）
  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | null = null
    listen<string>('whisper:stderr', (event) => {
      wlog(event.payload)
    }).then((fn) => {
      if (cancelled) fn()
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
          const text = normalizeTranscript(result.text, whisperLanguageRef.current)
          if (!text) {
            log('  片段辨識結果為空，略過')
          } else if (isWhisperHallucination(text)) {
            warn(`  偵測到幻覺字串，略過：「${text.slice(0, 60)}」`)
          } else {
            log(`✓ 片段辨識完成：「${text.slice(0, 60)}${text.length > 60 ? '…' : ''}」`)
            setSegmentsDone((n) => n + 1)
            // 加入 typewriter buffer，逐字顯示而非整段輸出
            enqueueTypewriter(text)
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
  }, [enqueueTypewriter])

  const enqueueChunk = useCallback((chunk: number[]) => {
    queueRef.current.push(chunk)
    void drainQueue()
  }, [drainQueue])

  // ─── Recording ──────────────────────────────────────────────────────────────

  const startRecording = useCallback(async () => {
    setErrorMsg('')
    setSegmentsDone(0)
    queueRef.current              = []
    processingRef.current         = false
    speechActiveRef.current       = false
    speechEverDetectedRef.current = false
    silenceSamplesRef.current     = 0
    chunkStartRef.current         = 0
    resetTypewriter()
    if (silenceWarnTimerRef.current !== null) { clearTimeout(silenceWarnTimerRef.current); silenceWarnTimerRef.current = null }
    if (autoStopTimerRef.current !== null)    { clearTimeout(autoStopTimerRef.current);    autoStopTimerRef.current = null }
    if (previewTimerRef.current !== null)     { clearInterval(previewTimerRef.current);    previewTimerRef.current = null }
    previewBusyRef.current = false
    previewGenRef.current  = 0
    // Chain B 前次殘留清理（防止 quick toggle）
    rawWorkletRef.current?.disconnect(); rawWorkletRef.current?.port.close(); rawWorkletRef.current = null
    void rawCtxRef.current?.close(); rawCtxRef.current = null
    rawPreviewSamplesRef.current = []; rawPreviewChunkStartRef.current = 0; previewTextAccumRef.current = ''

    rnnoiseStateRef.current?.destroy()
    rnnoiseStateRef.current = null
    const useRNNoise = noiseSuppressionRef.current
    log(`▶ 開始錄音（AudioWorklet，雜訊抑制: ${useRNNoise ? 'RNNoise@主執行緒' : '瀏覽器內建'}）`)
    try {
      // ── 裝置檢查：確認系統有 audioinput 設備 ─────────────────────────────
      const devices = await navigator.mediaDevices.enumerateDevices()
      const hasAudioInput = devices.some((d) => d.kind === 'audioinput')
      if (!hasAudioInput) {
        throw new Error('未找到麥克風裝置，請確認裝置已連接')
      }
      log(`✓ 找到 ${devices.filter(d => d.kind === 'audioinput').length} 個麥克風裝置`)

      // RNNoise 啟用：48kHz AudioContext；關閉：16kHz（不需降取樣）
      const contextSampleRate = useRNNoise ? RNNOISE_SAMPLE_RATE : WHISPER_SAMPLE_RATE
      const ctx    = new AudioContext({ sampleRate: contextSampleRate })
      // Chain B: 16kHz raw，專供 preview（不等 RNNoise WASM 載入）
      const rawCtx = new AudioContext({ sampleRate: WHISPER_SAMPLE_RATE })
      sampleRateRef.current = WHISPER_SAMPLE_RATE  // samplesRef 永遠儲存 16kHz 音頻
      samplesRef.current           = []
      rawPreviewSamplesRef.current  = []
      rawPreviewChunkStartRef.current = 0
      log(`AudioContext 實際 sampleRate: ${ctx.sampleRate} Hz（whisper 取樣率: ${WHISPER_SAMPLE_RATE} Hz）`)

      // 載入 AudioWorklet 模組（主鏈 + 預覽鏈並行，互不阻塞）
      await Promise.all([
        ctx.audioWorklet.addModule(WORKLET_URL),
        rawCtx.audioWorklet.addModule(WORKLET_URL),
      ])
      log('✓ AudioWorklet 模組載入完成')

      // VAD 門檻以 16kHz 樣本數計算（samplesRef 的實際取樣率）
      const silenceSamplesThreshold = Math.floor(SILENCE_DURATION_MS / 1000 * WHISPER_SAMPLE_RATE)
      const minSegmentSamples       = Math.floor(MIN_SEGMENT_SEC * WHISPER_SAMPLE_RATE)

      // WebKit (WKWebView on macOS) suspends AudioContext on creation;
      // resume() is required before the audio graph processes any data.
      if (ctx.state    === 'suspended') await ctx.resume()
      if (rawCtx.state === 'suspended') await rawCtx.resume()

      const worklet = new AudioWorkletNode(ctx, 'voice-processor')

      // WebKit / WKWebView will not invoke worklet.process() unless the node
      // is connected (directly or indirectly) to ctx.destination.
      // Use a zero-gain node so the mic audio is NOT played through speakers.
      const silentGain = ctx.createGain()
      silentGain.gain.value = 0
      worklet.connect(silentGain)
      silentGain.connect(ctx.destination)

      // ── VAD 邏輯 ─────────────────────────────────────────────────────────────
      const runVAD = (nowSpeaking: boolean, sampleCount: number, label: string) => {
        if (nowSpeaking) {
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
          silenceSamplesRef.current += sampleCount
          if (speechActiveRef.current && silenceSamplesRef.current >= silenceSamplesThreshold) {
            speechActiveRef.current = false
            silenceSamplesRef.current = 0
            setIsSpeaking(false)
            const endIdx = samplesRef.current.length
            const chunk  = samplesRef.current.slice(chunkStartRef.current, endIdx)
            if (chunk.length >= minSegmentSamples) {
              const rms = chunkRms(chunk)
              if (rms >= MIN_CHUNK_RMS) {
                log(`VAD: 沖出 ${(chunk.length / sampleRateRef.current).toFixed(1)}s 片段（${label}，RMS=${rms.toFixed(4)}）`)
                previewGenRef.current++
                onPreviewRef.current?.(null)
                enqueueChunk([...chunk])
              } else {
                log(`VAD: 片段 RMS=${rms.toFixed(4)} 低於門檻（${MIN_CHUNK_RMS}），疑似環境噪音，略過`)
              }
              // 無論是否送出，都推進 chunk 起點並重置 Chain B（讓 preview 從新位置累積）
              chunkStartRef.current = endIdx
              rawPreviewSamplesRef.current    = []
              rawPreviewChunkStartRef.current = 0
              previewTextAccumRef.current     = ''
            }
          }
        }
      }

      // ── worklet 訊息處理器 ────────────────────────────────────────────────────
      // 先接收一次 'ready' 訊息，確認 RNNoise 是否成功載入，
      // 之後立即切換為音頻資料處理器。
      // 錄音不等待 worklet ready 才開始：getUserMedia 完成就 setState('recording')，
      // worklet 尚未 ready 時 process() 靜默丟棄音頻（通常 < 200ms），
      // 幾乎不影響使用者體驗。
      // 主執行緒音頻處理器：接收 worklet 原始幀，執行 RNNoise（若已載入）+ 降取樣
      const handleAudio = (e: MessageEvent<{ samples: Float32Array }>) => {
        const frame = e.data.samples
        let pcm16: ArrayLike<number>
        let rms: number

        if (contextSampleRate > 16000) {
          // 48 kHz 路徑：縮放至 16 位元範圍 → RNNoise（若已載入）→ 3:1 降取樣 → 16 kHz
          for (let i = 0; i < frame.length; i++) frame[i] *= 32768
          rnnoiseStateRef.current?.processFrame(frame)
          const downCount = frame.length / DS_RATIO  // 480 / 3 = 160 samples @ 16 kHz
          const out = new Float32Array(downCount)
          let sumSq = 0
          for (let i = 0, j = 0; i < frame.length; i += DS_RATIO, j++) {
            const s = frame[i] / 32768
            out[j] = s
            sumSq += s * s
          }
          pcm16 = out
          rms = Math.sqrt(sumSq / downCount)
        } else {
          // 16 kHz 直通（RNNoise 關閉，AudioContext 已在 16 kHz）
          let sumSq = 0
          for (let i = 0; i < frame.length; i++) sumSq += frame[i] * frame[i]
          pcm16 = frame
          rms = Math.sqrt(sumSq / frame.length)
        }

        for (let i = 0; i < pcm16.length; i++) samplesRef.current.push(pcm16[i])
        runVAD(rms > RMS_THRESHOLD, pcm16.length, rnnoiseStateRef.current ? 'RNNoise@主執行緒' : '瀏覽器抑制')
      }
      worklet.port.onmessage = (e: MessageEvent<{ type?: string }>) => {
        if (e.data.type === 'ready') {
          worklet.port.onmessage = handleAudio   // 切換為音頻資料處理器
        }
      }

      // ── getUserMedia（唯一需要 await 的步驟）────────────────────────────────
      // worklet init 與 getUserMedia 同時執行；stream 一到位就立即開始錄音。
      worklet.port.postMessage({ type: 'init' })

      // 非同步載入 RNNoise WASM（主執行緒）；通常在第一個 VAD 片段被沖出前就完成
      if (useRNNoise) {
        ;(async () => {
          try {
            const mod = await loadRNNoiseModule()
            if (audioCtxRef.current) {   // 確認錄音仍在進行中
              rnnoiseStateRef.current = mod.createDenoiseState()
              log('✓ RNNoise WASM 在主執行緒載入完成')
            }
            // 若已停止錄音，DenoiseState 不建立即可；mod 本身已快取，不需釋放
          } catch (e) {
            warn(`RNNoise 主執行緒載入失敗：${e}，降級為僅降取樣`)
          }
        })()
      }

      const stream = await navigator.mediaDevices.getUserMedia({
        // RNNoise 啟用時，關閉瀏覽器內建抑制，由 RNNoise 全權處理
        audio: { channelCount: 1, echoCancellation: true, noiseSuppression: !useRNNoise },
      })
      log('✓ getUserMedia 成功')

      const source = ctx.createMediaStreamSource(stream)
      source.connect(worklet)

      streamRef.current   = stream
      audioCtxRef.current = ctx
      workletRef.current  = worklet

      // ── Chain B：16kHz raw，不跑 RNNoise，專供 preview 計時器 ────────────────
      // 因為 enabled:false，worklet 立即送回 'ready'，無 WASM 等待，
      // 音頻從 getUserMedia 成功瞬間就開始累積，短 interval 也能如期觸發預覽。
      const rawWorklet    = new AudioWorkletNode(rawCtx, 'voice-processor')
      const rawSilentGain = rawCtx.createGain()
      rawSilentGain.gain.value = 0
      rawWorklet.connect(rawSilentGain)
      rawSilentGain.connect(rawCtx.destination)
      rawWorklet.port.onmessage = (e: MessageEvent<{ type?: string }>) => {
        if (e.data.type === 'ready') {
          rawWorklet.port.onmessage = (ev: MessageEvent<{ samples: Float32Array }>) => {
            const { samples } = ev.data
            for (let i = 0; i < samples.length; i++) rawPreviewSamplesRef.current.push(samples[i])
          }
        }
      }
      rawWorklet.port.postMessage({ type: 'init' })
      rawCtx.createMediaStreamSource(stream).connect(rawWorklet)
      rawCtxRef.current    = rawCtx
      rawWorkletRef.current = rawWorklet

      // ── Preview interval ─────────────────────────────────────────────────────
      // 每隔 previewIntervalMs 把 Chain B「新增的」原始音頻送 Whisper 預覽。
      // 每次成功後推進 rawPreviewChunkStartRef，避免重送舊音頻造成 preview 頻繁跳動。
      // VAD flush 時重置整個 buffer，preview 結果由正式轉錄取代。
      previewTimerRef.current = setInterval(async () => {
        if (previewBusyRef.current) return
        if (!onPreviewRef.current) return

        const snapshotEnd = rawPreviewSamplesRef.current.length  // 快照當前末尾，避免競態
        const unprocessed = rawPreviewSamplesRef.current.slice(rawPreviewChunkStartRef.current, snapshotEnd)
        // 最低門檻隨 interval 縮放（interval × 0.7），避免短 interval 時一直跳過
        const minSec = Math.min(MIN_PREVIEW_SEC, previewIntervalMsRef.current / 1000 * 0.7)
        if (unprocessed.length < sampleRateRef.current * minSec) return

        previewBusyRef.current = true
        const myGen     = previewGenRef.current
        const prevAccum = previewTextAccumRef.current
        // 辨識進行中提示（VAD flush 時會被 onPreview(null) 覆蓋，無需特別清除）
        // 分隔符與語言一致：CJK → '…'，其餘 → ' …'
        const loadingText = prevAccum
          ? prevAccum + previewSep(whisperLanguageRef.current, prevAccum, '…') + '…'
          : '語音辨識中…'
        onPreviewRef.current?.(loadingText)

        try {
          const result = await invoke<{ text: string }>('transcribe_audio', {
            pcmData: [...unprocessed],
            sampleRate: sampleRateRef.current,
          })
          if (previewGenRef.current !== myGen) return  // VAD 已沖出，結果已過期
          const text = normalizeTranscript(result.text, whisperLanguageRef.current)
          if (text && !isWhisperHallucination(text)) {
            const sep = previewSep(whisperLanguageRef.current, previewTextAccumRef.current, text)
            previewTextAccumRef.current += sep + text
            log(`  預覽辨識（累積）：「${previewTextAccumRef.current.slice(0, 60)}…」`)
            onPreviewRef.current?.(previewTextAccumRef.current)
            rawPreviewChunkStartRef.current = snapshotEnd  // 推進，下次只送新增音頻
            // 壓縮：已處理的前端音頻不再需要，避免陣列無限增長
            if (rawPreviewChunkStartRef.current > COMPACT_THRESHOLD_SAMPLES) {
              rawPreviewSamplesRef.current = rawPreviewSamplesRef.current.slice(rawPreviewChunkStartRef.current)
              rawPreviewChunkStartRef.current = 0
            }
          } else {
            // 幻覺或空結果：恢復辨識前的顯示狀態
            onPreviewRef.current?.(prevAccum || null)
          }
        } catch {
          // preview 是 best-effort，忽略錯誤，恢復辨識前的顯示狀態
          onPreviewRef.current?.(prevAccum || null)
        } finally {
          previewBusyRef.current = false
        }
      }, previewIntervalMsRef.current)

      // 30 秒後若仍無聲音偵測，提示用戶確認麥克風是否已啟用
      silenceWarnTimerRef.current = setTimeout(() => {
        silenceWarnTimerRef.current = null
        if (!speechEverDetectedRef.current) {
          warn('持續靜音 30 秒，可能麥克風未啟用')
          toast.warning('未偵測到聲音，請確認麥克風裝置是否已啟用')
        }
      }, 30_000)

      // 若從未偵測到聲音，30 分鐘後自動停止（初始計時器；說話後會在 VAD 中重置）
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
  }, [enqueueChunk, resetTypewriter])

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
    // Chain B cleanup（先於 stream.stop，避免 disconnect after track stop）
    rawWorkletRef.current?.disconnect()
    rawWorkletRef.current?.port.close()
    stream.getTracks().forEach((t) => t.stop())
    await Promise.all([ctx.close(), rawCtxRef.current?.close()])
    // 釋放主執行緒 RNNoise DenoiseState
    rnnoiseStateRef.current?.destroy()
    rnnoiseStateRef.current = null

    streamRef.current   = null
    audioCtxRef.current = null
    workletRef.current  = null
    rawCtxRef.current    = null
    rawWorkletRef.current = null
    rawPreviewSamplesRef.current    = []
    rawPreviewChunkStartRef.current = 0
    previewTextAccumRef.current     = ''

    // 清除所有計時器
    if (silenceWarnTimerRef.current !== null) { clearTimeout(silenceWarnTimerRef.current);  silenceWarnTimerRef.current = null }
    if (autoStopTimerRef.current !== null)    { clearTimeout(autoStopTimerRef.current);     autoStopTimerRef.current = null }
    if (previewTimerRef.current !== null)     { clearInterval(previewTimerRef.current);     previewTimerRef.current = null }
    previewGenRef.current++             // 令牌失效，丟棄飛行中的 preview 結果
    onPreviewRef.current?.(null)        // 立即清除 overlay 預覽文字
    setIsSpeaking(false) // 停止錄音時重置 speaking 狀態

    const totalSamples = samplesRef.current.length
    const remaining    = samplesRef.current.slice(chunkStartRef.current)
    samplesRef.current = []
    chunkStartRef.current = 0
    speechActiveRef.current = false
    speechEverDetectedRef.current = false
    silenceSamplesRef.current = 0

    log(`⏹ 停止錄音，共 ${totalSamples} 樣本（${(totalSamples / sampleRateRef.current).toFixed(2)}s @ ${sampleRateRef.current}Hz）`)

    // 傳送剩餘音訊片段：
    //   VAD 正常 flush 後 remaining 是短尾音/靜音 → whisper 快速返回空字串，無害
    //   VAD 從未觸發（音量偏低）→ remaining 是整段錄音，必須傳送
    //   完全無聲（無麥克風）→ RMS 趨近 0，低於 MIN_CHUNK_RMS，略過
    if (remaining.length >= sampleRateRef.current * 0.3) {
      const rms = chunkRms(remaining)
      if (rms >= MIN_CHUNK_RMS) {
        log(`→ 剩餘 ${(remaining.length / sampleRateRef.current).toFixed(1)}s（RMS=${rms.toFixed(4)}），加入佇列`)
        enqueueChunk([...remaining])
      } else {
        log(`→ 剩餘片段 RMS=${rms.toFixed(4)} 低於門檻（${MIN_CHUNK_RMS}），視為靜音略過`)
      }
    }

    if (queueRef.current.length === 0 && !processingRef.current) {
      if (totalSamples < sampleRateRef.current * 0.3) {
        warn('錄音過短（< 0.3 秒），略過辨識')
      }
      // 即使 queue 空，typewriter 可能還在輸出最後一段 — 等待清空
      while (typewriterBufferRef.current.length > 0) {
        await new Promise<void>((r) => setTimeout(r, 50))
      }
      setState('idle')
      return
    }

    setState('transcribing')

    // 等待：(1) 音訊佇列處理完成 (2) typewriter buffer 全部輸出
    while (
      queueRef.current.length > 0 ||
      processingRef.current ||
      typewriterBufferRef.current.length > 0
    ) {
      await new Promise<void>((r) => setTimeout(r, 50))
    }

    setState('done')
    setTimeout(() => setState('idle'), 2000)
  }, [enqueueChunk])

  // ─── Toggle ─────────────────────────────────────────────────────────────────

  // 保持 stopRecordingRef 永遠指向最新的 stopRecording（供計時器回呼使用）
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
