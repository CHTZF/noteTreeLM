import { useState, useRef, useEffect, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { useSettingsStore } from '../../stores/settingsStore'
import { useAuthStore } from '../../stores/authStore'
import { useEditorStore } from '../../stores/editorStore'
import { useDebugStore } from '../../stores/debugStore'
import { toast } from '../common/Toast'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'
import VoiceOverlay from '../common/VoiceOverlay'
import ConversationList from './ConversationList'

interface Message {
  role: 'user' | 'assistant' | 'tool' | 'notice'
  content: string
  webRefs?: Array<{ path: string; title: string; excerpt: string }>
  savedWeb?: boolean
}

export default function ChatPanel({ liveChatActive = false, onActiveChange, onOpenNote }: { liveChatActive?: boolean; onActiveChange?: (active: boolean) => void; onOpenNote?: (path: string) => void }) {
  const { settings } = useSettingsStore()
  const { session } = useAuthStore()
  const { content: noteContent, currentPath } = useEditorStore()
  const { addLog } = useDebugStore()

  const [messages, setMessages] = useState<Message[]>([])
  const [input, setInput] = useState('')
  const [isStreaming, setIsStreaming] = useState(false)
  const [conversationId, setConversationId] = useState<string | null>(null)
  const [sidebarOpen, setSidebarOpen] = useState(true)

  // Notify parent when streaming state changes (for close confirmation)
  useEffect(() => { onActiveChange?.(isStreaming) }, [isStreaming])
  const [streamingText, setStreamingText] = useState('')
  const useNoteContext = !!settings.chat_auto_include_note
  const writeConfirmMode = (settings.write_confirm_mode ?? 'always') as 'always' | 'once' | 'never'
  const [pendingWriteDisplay, setPendingWriteDisplay] = useState<string | null>(null)
  const [pendingSearchMethod, setPendingSearchMethod] = useState<{ query: string } | null>(null)
  const [error, setError] = useState('')
  const [isCompressing, setIsCompressing] = useState(false)
  const [lastMemoryPath] = useState<string | null>(null)  // 保留供 send dep array
  const [showClearPrompt, setShowClearPrompt] = useState(false)
  const [savingWebMsgIdx, setSavingWebMsgIdx] = useState<number | null>(null)
  const [ratings, setRatings] = useState<Record<number, 'good' | 'bad'>>({})

  // ─── In-conversation skill creation ───────────────────────────────────────
  type SkillPreview = {
    userMsg: string
    assistantMsg: string
    loading: boolean
    title: string
    trigger: string
    behavior: string
    autoToolCalls: string[]
    injectionMode: 'passive' | 'active'
  }
  const [skillPreview, setSkillPreview] = useState<SkillPreview | null>(null)
  const [savingSkill, setSavingSkill] = useState(false)

  // ─── Voice overlay ────────────────────────────────────────────────────────────
  const [voiceTranscript, setVoiceTranscript]   = useState('')
  const [voicePreview, setVoicePreview]         = useState<string | null>(null)
  const [showVoiceOverlay, setShowVoiceOverlay] = useState(false)
  const pendingVoiceActionRef    = useRef<'confirm' | 'discard' | 'save_current' | 'save_new' | null>(null)
  const pendingVoiceSaveNewRef   = useRef<{ title: string; folder: string } | null>(null)

  const bottomRef = useRef<HTMLDivElement>(null)
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const streamingRef = useRef('')
  const tokenCountRef = useRef(0)
  const sessionWriteApprovedRef = useRef(false)
  const userMsgCountRef = useRef(0)
  // 追蹤上次 extract_memory_facts 時的訊息數量，避免 auto-save 與 clearChat 重複萃取
  const lastExtractedMsgCountRef = useRef(0)

  // Per-conversation draft state (input + chips), keyed by conversationId
  type DraftState = { input: string; noteSuggestions: { absPath: string; label: string }[] }
  const conversationDraftsRef = useRef<Record<string, DraftState>>({})
  const inputRef2 = useRef('')  // mirrors `input` state for save-on-switch (avoids stale closure)
  useEffect(() => { inputRef2.current = input }, [input])

  // Track note suggestions from agent:note_refs for "打開它" shortcut
  const [noteSuggestions, setNoteSuggestions] = useState<{ absPath: string; label: string }[]>([])
  const noteSuggestionsRef = useRef<{ absPath: string; label: string }[]>([])
  useEffect(() => { noteSuggestionsRef.current = noteSuggestions }, [noteSuggestions])

  // Track web refs from agent:web_refs (call_external_ai / web_search) for "儲存為知識"
  const pendingWebRefsRef = useRef<Array<{ path: string; title: string; excerpt: string }>>([])

  const log = useCallback((msg: string) => addLog('chat', 'info', msg), [addLog])
  const err = useCallback((msg: string) => addLog('chat', 'error', msg), [addLog])

  // 初始化：從 DB 恢復上次對話（不自動建立新對話）
  useEffect(() => {
    const username = session?.username ?? ''
    if (!username) return
    invoke<string | null>('get_last_chat_conversation_id', { username })
      .then(saved => {
        if (saved) {
          invoke('get_conversation', { id: saved })
            .then(() => { setConversationId(saved); loadConversationMessages(saved) })
            .catch(() => {
              // Stale ID — clear it and stay on empty screen
              invoke('set_last_chat_conversation_id', { username, conversationId: null }).catch(() => {})
            })
        }
      })
      .catch(() => {})
  }, [session?.username])

  const loadConversationMessages = useCallback(async (id: string) => {
    try {
      const [snap, ratingEntries] = await Promise.all([
        invoke<{ messages_json: string }>('get_conversation', { id }),
        invoke<Array<{ content_hash: string; rating: string }>>('get_conversation_ratings', { conversationId: id }).catch(() => [] as Array<{ content_hash: string; rating: string }>),
      ])
      const msgs: Array<{ role: string; content: string }> = JSON.parse(snap.messages_json)
      const filtered = msgs.filter(m => m.role === 'user' || m.role === 'assistant')
      const loadedMsgs = filtered.map(m => ({ role: m.role as 'user' | 'assistant', content: m.content }))
      setMessages(loadedMsgs)

      // 從 DB 恢復每則 assistant 回覆的評分狀態（依 content_hash 對位訊息 index）
      const ratingMap: Record<number, 'good' | 'bad'> = {}
      ratingEntries.forEach(({ content_hash, rating }) => {
        if (rating !== 'good' && rating !== 'bad') return
        const idx = loadedMsgs.findIndex(
          m => m.role === 'assistant' && m.content.slice(0, 150) === content_hash
        )
        if (idx >= 0) ratingMap[idx] = rating
      })
      setRatings(ratingMap)
    } catch (e) {
      console.error('[ChatPanel] loadConversationMessages error:', e)
      // Stale conversation ID — clear it and show empty screen
      const username = useAuthStore.getState().session?.username ?? ''
      invoke('set_last_chat_conversation_id', { username, conversationId: null }).catch(() => {})
      setConversationId(null)
      setMessages([])
      setRatings({})
    }
  }, [])

  // Save draft for the outgoing conversation, then restore (or clear) for the incoming one
  const switchConversation = useCallback((outgoingId: string | null, incomingId: string | null) => {
    // Save current draft
    if (outgoingId) {
      conversationDraftsRef.current[outgoingId] = {
        input: inputRef2.current,
        noteSuggestions: noteSuggestionsRef.current,
      }
    }
    // Restore draft for the new conversation (or start fresh)
    const draft = incomingId ? conversationDraftsRef.current[incomingId] : undefined
    setInput(draft?.input ?? '')
    setNoteSuggestions(draft?.noteSuggestions ?? [])
    noteSuggestionsRef.current = draft?.noteSuggestions ?? []
    setPendingWriteDisplay(null)
    setError('')
    setStreamingText('')
    streamingRef.current = ''
  }, [])

  const handleSelectConversation = useCallback((id: string) => {
    if (isStreaming) return
    switchConversation(conversationId, id)
    setConversationId(id)
    const username = useAuthStore.getState().session?.username ?? ''
    invoke('set_last_chat_conversation_id', { username, conversationId: id }).catch(() => {})
    loadConversationMessages(id)
  }, [isStreaming, conversationId, loadConversationMessages, switchConversation])

  const handleNewConversation = useCallback((id: string) => {
    const username = useAuthStore.getState().session?.username ?? ''
    if (!id) {
      // Current conversation was deleted — reset to empty screen
      switchConversation(conversationId, null)
      setConversationId(null)
      invoke('set_last_chat_conversation_id', { username, conversationId: null }).catch(() => {})
      setMessages([])
      setRatings({})
      return
    }
    switchConversation(conversationId, id)
    setConversationId(id)
    invoke('set_last_chat_conversation_id', { username, conversationId: id }).catch(() => {})
    setMessages([])
    setRatings({})
  }, [conversationId, switchConversation])

  const isConfigured = !!settings.llama_cli_path && !!settings.llm_model_path
  const whisperConfigured = !!settings.whisper_cli_path && !!settings.whisper_model_path
  // 語音轉文字：逐字累積到 overlay buffer（不直接寫入輸入框）
  const handleTranscript = useCallback((text: string) => {
    if (!text) return
    setVoiceTranscript((prev) => prev + text)
  }, [])
  // 臨時預覽：null = 清除，string = 更新預覽文字
  const handlePreview = useCallback((text: string | null) => {
    setVoicePreview(text)
  }, [])
  const previewEnabled          = settings.voice_preview_enabled !== false
  const noiseSuppressionEnabled = settings.voice_noise_suppression !== false
  const previewIntervalMs       = settings.voice_preview_interval ?? 5000
  const whisperLanguage         = settings.whisper_language ?? 'auto'
  const { state: voiceState, isSpeaking: voiceIsSpeaking, toggle: toggleVoice } = useVoiceRecorder(
    handleTranscript,
    previewEnabled ? handlePreview : undefined,
    noiseSuppressionEnabled,
    previewIntervalMs,
    whisperLanguage,
  )

  // 錄音開始時顯示 overlay，並重置轉錄文字與預覽
  useEffect(() => {
    if (voiceState === 'recording') {
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(true)
    }
  }, [voiceState])

  // 語音後處理：依 voice_process_mode 設定，用 llama 潤稿或加 wikilink
  const applyVoicePostProcess = useCallback(async (text: string): Promise<string> => {
    const mode = settings.voice_process_mode ?? 'none'
    if (mode !== 'none' && settings.llama_cli_path) {
      const system = mode === 'format'
        ? '你是文字整理助手。你的唯一任務是將輸入的口語錄音文字整理成通順的書面文字，保留原意，不要增加任何新內容。直接輸出整理後的文字，不要問問題、不要說明、不要對話。'
        : '你是筆記助手。你的唯一任務是分析輸入文字中的關鍵主題或概念，並將原始文字中出現的這些主題以 Obsidian wikilink 格式包圍（例如 [[主題]]）。直接輸出替換後的原始文字，不要增加任何新內容，不要問問題、不要說明、不要對話。'
      toast.info('llama 後處理中…')
      try {
        const result = await invoke<string>('process_with_llm', { system, userContent: text })
        return result.trim()
      } catch (e: any) {
        const msg = e?.AI ?? (typeof e === 'string' ? e : JSON.stringify(e))
        toast.error('後處理失敗：' + msg)
        return text // fallback：使用原始轉錄
      }
    }
    return text
  }, [settings.voice_process_mode, settings.llama_cli_path])

  // 寫入當前筆記（讀最新 editorStore state，避免 closure 舊值）
  const doSaveToCurrentNote = useCallback(async (text: string) => {
    const processedText = await applyVoicePostProcess(text)
    const { currentPath: path, content: existing } = useEditorStore.getState()
    if (!path) { toast.error('請先開啟一個筆記'); return }
    const newContent = existing ? existing + '\n\n' + processedText : processedText
    try {
      await invoke('update_note', { path, content: newContent })
      // 同步 Editor CM6 視圖，防止 auto-save 用舊內容覆蓋
      useEditorStore.getState().applyExternalWrite(newContent)
      toast.success('已追加寫入當前筆記')
    } catch (e) {
      toast.error('寫入失敗：' + String(e))
    }
  }, [applyVoicePostProcess])

  // 建立新筆記（title / folder 由 VoiceOverlay 表單傳入）
  const doSaveToNewNote = useCallback(async (text: string, title: string, folder: string) => {
    const processedText = await applyVoicePostProcess(text)
    try {
      await invoke('create_note', { title, folder: folder || null, content: processedText })
      toast.success(`已建立筆記「${title}」`)
    } catch (e) {
      toast.error('建立失敗：' + String(e))
    }
  }, [applyVoicePostProcess])

  // 有待執行動作（confirm/discard/save_*）時，等辨識完成後執行
  useEffect(() => {
    const action = pendingVoiceActionRef.current
    if (!action) return
    if (voiceState === 'done' || voiceState === 'idle') {
      pendingVoiceActionRef.current = null
      const text = voiceTranscript
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
      if (action === 'confirm' && text) {
        setInput((prev) => {
          const sep = prev && !prev.endsWith(' ') ? ' ' : ''
          return prev + sep + text
        })
      } else if (action === 'save_current' && text) {
        doSaveToCurrentNote(text)
      } else if (action === 'save_new' && text) {
        const saveData = pendingVoiceSaveNewRef.current
        pendingVoiceSaveNewRef.current = null
        if (saveData) doSaveToNewNote(text, saveData.title, saveData.folder)
      }
    }
  }, [voiceState, voiceTranscript, doSaveToCurrentNote, doSaveToNewNote])

  // 語音帶入時自動調整 textarea 高度
  useEffect(() => {
    const t = inputRef.current
    if (!t) return
    t.style.height = 'auto'
    t.style.height = Math.min(t.scrollHeight, 120) + 'px'
    t.scrollTop = t.scrollHeight
  }, [input])

  // 持續監聽 llm:stderr → 寫入 debug
  // 使用 cancelled flag 解決 React StrictMode 下 async listen() 的 race condition
  // （cleanup 可能在 Promise resolve 前執行，導致舊 listener 未被移除而重複觸發）
  useEffect(() => {
    let cancelled = false
    let unlisten: (() => void) | undefined
    listen<string>('llm:stderr', (event) => {
      useDebugStore.getState().addLog('llm', 'warn', event.payload.trimEnd())
    }).then((fn) => {
      if (cancelled) fn() // 已 cleanup，立即取消這個 listener
      else unlisten = fn
    })
    return () => {
      cancelled = true
      unlisten?.()
    }
  }, []) // addLog 是 Zustand stable reference，不需要作為 dep

  // 持續監聽 sub_agent:* 事件 → 寫入 debug
  useEffect(() => {
    let cancelled = false
    const unlistens: (() => void)[] = []

    const setup = async () => {
      const { addLog: dbgLog } = useDebugStore.getState()

      const fn1 = await listen<{ sub_session_id: string; kind: string; task: string }>(
        'sub_agent:start', (e) => {
          if (cancelled) return
          dbgLog('sub-agent', 'info', `🤖 [${e.payload.kind}] 開始：${e.payload.task.slice(0, 80)}`)
        }
      )
      const fn2 = await listen<{ sub_session_id: string; kind: string; display: string }>(
        'sub_agent:tool_call', (e) => {
          if (cancelled) return
          dbgLog('sub-agent', 'info', e.payload.display)
        }
      )
      const fn3 = await listen<{ sub_session_id: string; kind: string; result_preview: string }>(
        'sub_agent:done', (e) => {
          if (cancelled) return
          dbgLog('sub-agent', 'info', `✅ [${e.payload.kind}] 完成`)
        }
      )
      const fn4 = await listen<{ sub_session_id: string; error: string }>(
        'sub_agent:error', (e) => {
          if (cancelled) return
          dbgLog('sub-agent', 'error', `❌ sub-agent 錯誤：${e.payload.error}`)
        }
      )
      if (cancelled) { fn1(); fn2(); fn3(); fn4(); return }
      unlistens.push(fn1, fn2, fn3, fn4)
    }

    setup()
    return () => {
      cancelled = true
      unlistens.forEach(fn => fn())
    }
  }, [])

  // 自動捲動到底部
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages.length, streamingText])

  // (lastMemoryPath 保留供 send dep array，但不再主動設定——記憶改走 compress_conversation_to_knowledge)

  const handleWriteConfirm = useCallback(async (approved: boolean) => {
    if (approved && writeConfirmMode === 'once') {
      sessionWriteApprovedRef.current = true
    }
    setPendingWriteDisplay(null)
    await invoke('confirm_write_tool', { approved })
  }, [writeConfirmMode])

  const handleSearchMethodChoice = useCallback(async (method: string) => {
    setPendingSearchMethod(null)
    await invoke('confirm_search_method', { method })
  }, [])

  const send = useCallback(async () => {
    const text = input.trim()
    if (!text || isStreaming) return

    const userMsg: Message = { role: 'user', content: text }
    const allMessages = [...messages, userMsg]
    setMessages(allMessages)
    setInput('')
    setIsStreaming(true)
    setStreamingText('')
    setError('')
    streamingRef.current = ''
    tokenCountRef.current = 0

    // 只把 user/assistant 訊息送給 LLM（tool/notice 只做 UI 顯示，不進入 context）
    const llmMessages = allMessages
      .filter((m) => m.role === 'user' || m.role === 'assistant')
      .map((m) => ({ role: m.role, content: m.content }))

    log(`▶ 傳送訊息（${text.length} 字）`)

    let unlistenToken: (() => void) | undefined
    let unlistenDone: (() => void) | undefined
    let unlistenToolCall: (() => void) | undefined
    let unlistenWriteReq: (() => void) | undefined
    let unlistenNoteRefs: (() => void) | undefined
    let unlistenOpenNote: (() => void) | undefined
    let unlistenSearchMethod: (() => void) | undefined
    let unlistenWebRefs: (() => void) | undefined
    let unlistenSkillsActivated: (() => void) = () => {}
    let unlistenSkillSuggestion: (() => void) = () => {}
    let unlistenPreRouteDebug: (() => void) | undefined

    // Clear previous suggestions at the start of each send
    setNoteSuggestions([])
    noteSuggestionsRef.current = []
    pendingWebRefsRef.current = []

    try {
      const ORCHESTRATOR_SYSTEM =
        `你是一個筆記助理，可以直接使用工具完成使用者的請求。\n` +
        `可用工具：\n` +
        `- 讀取：search_vault、read_note、list_structure、list_notes_in_folder、query_memory、get_current_datetime\n` +
        `- 開啟：open_note\n` +
        `- 寫入：create_note、update_note、append_to_note、create_folder\n` +
        `- 刪除/移動：delete_note、delete_folder、move_note\n` +
        `- 搜尋：search_web\n` +
        `- 外部 AI：call_external_ai\n` +
        `- 排程：schedule_task\n` +
        `- UI：show_toast、ui_action\n` +
        `- 反思：reflect_on_skills\n` +
        `- 對話記錄：list_recent_conversations\n` +
        `規則：\n` +
        `1. 使用者要「打開」某筆記 → 先 search_vault 找到路徑，再 open_note 打開。\n` +
        `2. 使用者要「搜尋」或「查看內容」→ search_vault 再 read_note。\n` +
        `3. 需要即時網路資訊 → search_web；需要 AI 分析 → call_external_ai。\n` +
        `4. 禁止虛構筆記名稱或路徑；搜尋無結果時直接告知找不到。\n` +
        `5. 純閒聊或解釋概念可不使用工具。`

      const notePart =
        useNoteContext && currentPath && noteContent
          ? `你是一個筆記助手。以下是使用者目前開啟的筆記內容，請根據此內容協助回答問題：\n\n${noteContent.slice(0, 4000)}`
          : null

      // 記憶上下文由 agent 內部自動預取注入（prefetch_memory），前端不需再呼叫 resolve_memory_context
      const system = notePart
        ? `${ORCHESTRATOR_SYSTEM}\n\n${notePart}`
        : ORCHESTRATOR_SYSTEM
      if (system) log(`  帶入 system 上下文（${system.length} 字元）`)

      // 監聽工具調用顯示
      unlistenToolCall = await listen<{ session_id?: string; display?: string } | string>('agent:tool_call', (event) => {
        const display = typeof event.payload === 'string'
          ? event.payload
          : (event.payload as any)?.display ?? JSON.stringify(event.payload)
        setMessages((prev) => [...prev, { role: 'tool', content: display }])
      })

      // 追蹤筆記建議（供 chip 按鈕顯示）
      unlistenNoteRefs = await listen<string[]>('agent:note_refs', (e) => {
        const suggestions = e.payload.map(absPath => {
          const hashIdx = absPath.indexOf('#')
          const filePart = hashIdx >= 0 ? absPath.slice(0, hashIdx) : absPath
          const section = hashIdx >= 0 ? absPath.slice(hashIdx + 1) : ''
          const filename = filePart.split('/').pop()?.replace(/\.md$/, '') ?? filePart
          const label = section ? `${filename} § ${section}` : filename
          return { absPath, label }
        })
        setNoteSuggestions(suggestions)
        noteSuggestionsRef.current = suggestions
      })

      // 後端 pending_plan 確認後開啟筆記（embedding-based intent classify）
      unlistenOpenNote = await listen<string[]>('agent:open_note', (e) => {
        if (onOpenNote && e.payload.length > 0) {
          onOpenNote(e.payload[0])
        }
      })

      // 監聽寫入確認請求
      unlistenWriteReq = await listen<string>('agent:write_request', async (event) => {
        const display = event.payload
        if (writeConfirmMode === 'never') {
          await invoke('confirm_write_tool', { approved: true })
          return
        }
        if (writeConfirmMode === 'once' && sessionWriteApprovedRef.current) {
          await invoke('confirm_write_tool', { approved: true })
          return
        }
        // 'always' 或 'once' 首次：顯示確認 UI
        setPendingWriteDisplay(display)
      })

      // 監聽搜尋方式選擇請求
      unlistenSearchMethod = await listen<{ query: string }>('agent:search_method_request', (event) => {
        setPendingSearchMethod(event.payload)
      })

      unlistenToken = await listen<string>('llm:token', (event) => {
        streamingRef.current += event.payload
        tokenCountRef.current += event.payload.length
        setStreamingText(streamingRef.current)
        if (tokenCountRef.current === event.payload.length) {
          log('✓ 開始收到 llm:token 串流')
        }
      })
      unlistenDone = await listen('llm:done', () => {
        log(`⏹ llm:done 事件收到，共 ${tokenCountRef.current} 字元`)
      })
      unlistenWebRefs = await listen<Array<{ path: string; title: string; excerpt: string }>>('agent:web_refs', (e) => {
        pendingWebRefsRef.current = [...pendingWebRefsRef.current, ...e.payload]
      })

      // 透明度：skill pre-pass 觸發時，顯示哪些技能正在作用
      unlistenSkillsActivated = await listen<{ titles: string[] }>(
        'agent:skills_activated',
        (e) => {
          const names = e.payload.titles.join('、')
          setMessages(prev => [...prev, {
            role: 'tool' as const,
            content: `⚡ 套用技能：${names}`,
          }])
          unlistenSkillsActivated()
        }
      )

      // Bottom-up skill 歸納：agent 偵測到結構化回答框架時，提示使用者存為技能規範
      unlistenSkillSuggestion = await listen<{ query: string; response_preview: string }>(
        'agent:skill_suggestion',
        (_e) => {
          setMessages(prev => [...prev, {
            role: 'notice' as const,
            content: `💡 這個回答包含可重用的框架，是否前往「知識中心 → 我的技能規範」將此偏好設定為技能？`,
          }])
          unlistenSkillSuggestion()
        }
      )

      // Pre-routing debug trace
      unlistenPreRouteDebug = await listen<{ step: string; reason?: string; best_match?: string; dim?: number; found?: number; repaired?: number }>(
        'agent:pre_route_debug',
        (e) => {
          const p = e.payload
          const msg = p.step === 'skip'
            ? `⚠ pre-routing skip: ${p.reason}`
            : p.step === 'miss'
            ? `⚠ pre-routing miss: ${p.reason}${p.best_match ? ` (best: ${p.best_match})` : ''}`
            : p.step === 'repair_done'
            ? `✓ pre-routing repair_done found=${p.found} repaired=${p.repaired}`
            : p.step === 'create_agent_called'
            ? `🔨 create_agent called: name=${(p as any).name} trigger=${(p as any).trigger} emb=${(p as any).emb_url}`
            : p.step === 'db_dump'
            ? `🗄 db_dump vid=${(p as any).query_vid} | ${(p as any).agents?.map((a: any) => `${a.name}[t=${a.trigger},emb=${a.has_emb},st=${a.status},vid=${a.vid_match?'✓':'✗'},builtin=${a.builtin}]`).join(' | ') || '(no agents)'}`
            : `✓ pre-routing ${p.step}${p.dim ? ` dim=${p.dim}` : ''}`
          addLog('llm', 'info', msg)
        }
      )

      log('  呼叫 invoke("invoke_agent")')
      const responseText = await invoke<string>('invoke_agent', {
        input: text,
        messages: llmMessages,
        system,
        conversationId: conversationId ?? undefined,
      })

      const finalContent = responseText || streamingRef.current
      log(`✓ invoke_agent 完成，回覆 ${finalContent.length} 字元`)
      const webRefs = pendingWebRefsRef.current.length > 0
        ? pendingWebRefsRef.current
        : undefined
      setMessages((prev) => [...prev, { role: 'assistant', content: finalContent, webRefs }])

      // 自動偵測：使用者訊息含技能意圖關鍵字 → 自動觸發技能萃取
      if (hasSkillIntent(text) && finalContent.length > 0) {
        doExtractSkill(text, finalContent)
      }

      // Reflection：每 N 則對話後背景觀察模式（不阻塞 UI）
      userMsgCountRef.current += 1
      if (userMsgCountRef.current % REFLECTION_INTERVAL === 0) {
        triggerReflection()
      }

      // 每 memory_threshold 則訊息自動快照記憶（app 關閉前也能保留）
      const autoSaveThreshold = settings.memory_threshold ?? 20
      if (userMsgCountRef.current > 0 && userMsgCountRef.current % autoSaveThreshold === 0) {
        const snapshotMsgs = [...messages, { role: 'user' as const, content: text },
          { role: 'assistant' as const, content: finalContent }]
          .filter(m => m.role === 'user' || m.role === 'assistant')
          .map(m => ({ role: m.role, content: m.content }))
        if (snapshotMsgs.length >= 2) {
          invoke('save_memory_session', { messages: snapshotMsgs }).catch(() => {})
          lastExtractedMsgCountRef.current = snapshotMsgs.length
          invoke('extract_memory_facts', { messages: snapshotMsgs }).catch(() => {})
        }
      }
    } catch (e: unknown) {
      const msg =
        typeof e === 'string'
          ? e
          : e instanceof Error
            ? e.message
            : (() => { try { return JSON.stringify(e, null, 2) } catch { return String(e) } })()
      err(`invoke 失敗：\n${msg}`)
      setError(msg)
    } finally {
      unlistenToken?.()
      unlistenDone?.()
      unlistenToolCall?.()
      unlistenWriteReq?.()
      unlistenNoteRefs?.()
      unlistenOpenNote?.()
      unlistenSearchMethod?.()
      unlistenWebRefs?.()
      unlistenSkillsActivated()
      unlistenSkillSuggestion()
      unlistenPreRouteDebug?.()
      setPendingWriteDisplay(null)
      setPendingSearchMethod(null)
      setIsStreaming(false)
      setStreamingText('')
      streamingRef.current = ''
    }
  }, [input, isStreaming, messages, useNoteContext, writeConfirmMode, currentPath, noteContent, lastMemoryPath, conversationId, onOpenNote, log, err])

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      send()
    }
  }

  const clearChat = () => {
    // 有意義的對話（至少 1 則 user + 1 則 assistant）才儲存記憶
    const meaningfulMsgs = messages.filter(m => m.role === 'user' || m.role === 'assistant')
    if (meaningfulMsgs.length >= 2) {
      const chatMessages = meaningfulMsgs.map(m => ({ role: m.role, content: m.content }))
      // 若 auto-save 距今不足 4 條新訊息則跳過 extract（避免重複萃取同份資料）
      const newSinceLastExtract = chatMessages.length - lastExtractedMsgCountRef.current
      invoke('save_memory_session', { messages: chatMessages })
        .then(() => newSinceLastExtract >= 4
          ? invoke('extract_memory_facts', { messages: chatMessages }).catch(() => {})
          : Promise.resolve()
        )
        .then(() => invoke('distill_preferences').catch(() => {}))
        .then(() => invoke('analyze_tool_patterns').catch(() => {}))
        .catch(() => {})
      lastExtractedMsgCountRef.current = 0
    }
    setRatings({})
    setMessages([])
    setError('')
    setStreamingText('')
    streamingRef.current = ''
    setShowClearPrompt(false)
  }

  // ─── Reflection：每 6 則訊息後背景觀察對話模式，結果顯示在 Debug tab ──────
  const REFLECTION_INTERVAL = 6
  const REFLECTION_SYSTEM = `你是一個自我改進系統，負責觀察使用者與 AI 的對話模式並自動固化知識。
執行步驟：
1. 呼叫 list_recent_conversations 讀取最近對話
2. 分析重複需求、知識缺口、常用查詢模式
3. 若發現可固化的行為模式，呼叫 create_agent_skill 建立技能規範
4. 若有值得保存的知識，呼叫 create_note 建立筆記
限制：技能規範預設未啟用，使用者可在「我的技能規範」頁面審核。不要產生冗餘或過度通泛的技能。`

  const triggerReflection = useCallback(async () => {
    const { addLog: dbgLog } = useDebugStore.getState()
    dbgLog('reflection', 'info', '🔄 開始觀察對話模式…')

    let unlistenTool: (() => void) | null = null

    try {
      unlistenTool = await listen<{ display: string }>('agent:tool_call', e => {
        dbgLog('reflection', 'info', e.payload.display)
      })

      // invoke_agent 是 blocking — await 到 agent loop 完成才 resolve
      const result = await invoke<string>('invoke_agent', {
        input: '開始觀察',
        messages: [],
        system: REFLECTION_SYSTEM,
        useTools: true,
      })

      if (result) dbgLog('reflection', 'info', result)
      dbgLog('reflection', 'info', '✅ 觀察完成')
      window.dispatchEvent(new CustomEvent('skills-changed'))
    } catch (e) {
      dbgLog('reflection', 'error', `觸發失敗：${e}`)
    } finally {
      unlistenTool?.()
    }
  }, [])

  const handleSaveWeb = useCallback(async (msgIndex: number, msg: Message) => {
    if (!msg.webRefs?.length) return
    setSavingWebMsgIdx(msgIndex)
    try {
      const title = msg.webRefs[0]?.title || '知識項目'
      await invoke('save_knowledge_item', {
        sessionId: 'chat',
        title,
        aiSummary: msg.content,
        sourceRefs: msg.webRefs,
      })
      setMessages(prev => prev.map((m, i) => i === msgIndex ? { ...m, savedWeb: true } : m))
      toast.success(`已儲存「${title}」`)
    } catch (e: unknown) {
      const errMsg = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)
      toast.error(`儲存失敗：${errMsg}`)
    } finally {
      setSavingWebMsgIdx(null)
    }
  }, [])

  // 填入：若仍在錄音/辨識中則先停止，完成後填入輸入框
  const handleVoiceConfirm = useCallback(() => {
    if (voiceState === 'recording') {
      pendingVoiceActionRef.current = 'confirm'
      toggleVoice()
    } else if (voiceState === 'transcribing') {
      pendingVoiceActionRef.current = 'confirm'
    } else {
      const text = voiceTranscript
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
      if (text) {
        setInput((prev) => {
          const sep = prev && !prev.endsWith(' ') ? ' ' : ''
          return prev + sep + text
        })
      }
    }
  }, [voiceState, voiceTranscript, toggleVoice])

  // 捨棄：若仍在錄音/辨識中則先停止，完成後直接關閉 overlay
  const handleVoiceDiscard = useCallback(() => {
    if (voiceState === 'recording') {
      pendingVoiceActionRef.current = 'discard'
      toggleVoice()
    } else if (voiceState === 'transcribing') {
      pendingVoiceActionRef.current = 'discard'
    } else {
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
    }
  }, [voiceState, toggleVoice])

  // 寫入當前筆記：停止後追加轉錄文字
  const handleVoiceSaveToCurrentNote = useCallback(() => {
    if (voiceState === 'recording') {
      pendingVoiceActionRef.current = 'save_current'
      toggleVoice()
    } else if (voiceState === 'transcribing') {
      pendingVoiceActionRef.current = 'save_current'
    } else {
      const text = voiceTranscript
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
      if (text) doSaveToCurrentNote(text)
    }
  }, [voiceState, voiceTranscript, toggleVoice, doSaveToCurrentNote])

  // 建立新筆記：title / folder 由 VoiceOverlay 表單傳入
  const handleVoiceSaveToNewNote = useCallback((title: string, folder: string) => {
    if (voiceState === 'recording') {
      pendingVoiceSaveNewRef.current = { title, folder }
      pendingVoiceActionRef.current = 'save_new'
      toggleVoice()
    } else if (voiceState === 'transcribing') {
      pendingVoiceSaveNewRef.current = { title, folder }
      pendingVoiceActionRef.current = 'save_new'
    } else {
      const text = voiceTranscript
      setVoiceTranscript('')
      setVoicePreview(null)
      setShowVoiceOverlay(false)
      if (text) doSaveToNewNote(text, title, folder)
    }
  }, [voiceState, voiceTranscript, toggleVoice, doSaveToNewNote])

  // ── Skill intent keywords ─────────────────────────────────────────────────
  const SKILL_INTENT_KEYWORDS = ['記住', '以後', '從現在起', '幫我記', '每次都', '記得', '永遠', '不要再', '以後都', '下次', '請記得', '請幫我記']
  const hasSkillIntent = (text: string) => SKILL_INTENT_KEYWORDS.some(k => text.includes(k))

  // 呼叫 LLM 萃取技能（可由關鍵字自動觸發或使用者點擊📌按鈕觸發）
  const doExtractSkill = useCallback(async (userMsg: string, assistantMsg: string) => {
    setSkillPreview({ userMsg, assistantMsg, loading: true, title: '', trigger: '', behavior: '', autoToolCalls: [], injectionMode: 'passive' })
    try {
      const skill = await invoke<{ title: string; trigger: string; behavior: string; auto_tool_calls: string[] }>(
        'extract_skill_from_exchange', { userMsg, assistantMsg }
      )
      setSkillPreview({ userMsg, assistantMsg, loading: false, title: skill.title, trigger: skill.trigger, behavior: skill.behavior, autoToolCalls: skill.auto_tool_calls, injectionMode: 'passive' })
    } catch (e) {
      toast.error('技能萃取失敗：' + String(e))
      setSkillPreview(null)
    }
  }, [])

  // 確認建立技能規範
  const confirmSkill = useCallback(async () => {
    if (!skillPreview || skillPreview.loading) return
    setSavingSkill(true)
    try {
      await invoke('save_agent_skill', {
        knowledgeItemId: 'conversation',
        title: skillPreview.title,
        trigger: skillPreview.trigger,
        behavior: skillPreview.behavior,
        autoToolCalls: skillPreview.autoToolCalls,
        injectionMode: skillPreview.injectionMode,
      })
      toast.success(`技能「${skillPreview.title}」已建立`)
      setSkillPreview(null)
    } catch (e) {
      toast.error('建立失敗：' + String(e))
    } finally {
      setSavingSkill(false)
    }
  }, [skillPreview])

  // 壓縮記憶：用 LLM skill-first 萃取，建立 knowledge item + skill 規範
  const compressToKnowledge = useCallback(async (thenClear = false) => {
    const toCompress = messages.filter(m => m.role === 'user' || m.role === 'assistant')
    if (toCompress.length === 0 || isCompressing) return
    setIsCompressing(true)
    setShowClearPrompt(false)
    try {
      const result = await invoke<{ item_id: string; title: string; skill_count: number }>(
        'compress_conversation_to_knowledge',
        { messagesJson: JSON.stringify(toCompress) }
      )
      const skillMsg = result.skill_count > 0 ? `，萃取了 ${result.skill_count} 條技能規範` : ''
      const notice: Message = {
        role: 'notice',
        content: `已壓縮為知識「${result.title}」${skillMsg}。前往「知識中心」可查看技能建議。`,
      }
      if (thenClear) {
        setMessages([notice])
      } else {
        setMessages(prev => [...prev, notice])
      }
      toast.success(`知識已建立：${result.title}`)
      log(`compress_conversation_to_knowledge 完成：item_id=${result.item_id}`)
    } catch (e: unknown) {
      const msg = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)
      toast.error('壓縮失敗：' + msg)
      err('compress_conversation_to_knowledge 失敗：' + msg)
    } finally {
      setIsCompressing(false)
    }
  }, [messages, isCompressing, log, err])

  return (
    <div style={{ position: 'relative', display: 'flex', flexDirection: 'row', width: '100%', height: '100%', background: 'var(--color-bg-base)', overflow: 'hidden' }}>
      {/* Conversation sidebar */}
      {sidebarOpen && (
        <div style={{
          width: '180px', flexShrink: 0, borderRight: '1px solid var(--color-border)',
          display: 'flex', flexDirection: 'column', overflow: 'hidden',
        }}>
          <ConversationList
            mode="chat"
            selectedId={conversationId}
            onSelect={handleSelectConversation}
            onNew={handleNewConversation}
          />
        </div>
      )}

      {/* Main chat area */}
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden', minWidth: 0 }}>

        {/* Persistent header — always visible */}
        <div style={{
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          padding: '8px 12px', borderBottom: '1px solid var(--color-border)', flexShrink: 0,
        }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
            <button
              onClick={() => setSidebarOpen(o => !o)}
              title={sidebarOpen ? '收合對話列表' : '展開對話列表'}
              style={{
                width: 24, height: 24, display: 'flex', alignItems: 'center', justifyContent: 'center',
                background: 'transparent', border: '1px solid var(--color-border)', borderRadius: '4px',
                color: 'var(--color-text-muted)', cursor: 'pointer', fontSize: '12px', padding: 0, flexShrink: 0,
              }}
            >
              {sidebarOpen
                ? <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><line x1="3" y1="6" x2="21" y2="6"/><line x1="3" y1="12" x2="21" y2="12"/><line x1="3" y1="18" x2="21" y2="18"/></svg>
                : <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><line x1="3" y1="6" x2="21" y2="6"/><line x1="3" y1="12" x2="21" y2="12"/><line x1="3" y1="18" x2="21" y2="18"/></svg>
              }
            </button>
            <span style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)', letterSpacing: '0.05em' }}>CHAT</span>
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: '10px' }}>
            {conversationId && messages.length > 0 && (
              <>
                <button
                  onClick={() => compressToKnowledge(false)}
                  disabled={isCompressing || isStreaming}
                  title="用 AI 萃取對話精華，建立知識項目與技能規範"
                  style={{
                    fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                    background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
                    color: isCompressing ? 'var(--color-text-muted)' : 'var(--color-accent)',
                    cursor: isCompressing || isStreaming ? 'not-allowed' : 'pointer',
                    opacity: isCompressing || isStreaming ? 0.5 : 1,
                  }}
                >{isCompressing ? '壓縮中…' : '壓縮記憶'}</button>
                {showClearPrompt ? (
                  <div style={{ display: 'flex', alignItems: 'center', gap: '4px' }}>
                    <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>要先壓縮？</span>
                    <button
                      onClick={() => compressToKnowledge(true)}
                      disabled={isCompressing}
                      style={{ fontSize: '11px', padding: '2px 8px', borderRadius: '4px', background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer' }}
                    >壓縮後清除</button>
                    <button
                      onClick={clearChat}
                      style={{ fontSize: '11px', padding: '2px 8px', borderRadius: '4px', background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)', color: 'var(--color-text-secondary)', cursor: 'pointer' }}
                    >直接清除</button>
                  </div>
                ) : (
                <button
                  onClick={() => messages.filter(m => m.role === 'user' || m.role === 'assistant').length > 0 ? setShowClearPrompt(true) : clearChat()}
                  style={{
                    fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                    background: 'var(--color-bg-overlay)', border: '1px solid var(--color-border)',
                    color: 'var(--color-text-secondary)', cursor: 'pointer',
                  }}
                >清除</button>
                )}
              </>
            )}
          </div>
        </div>

      {/* No conversation selected — placeholder */}
      {!conversationId && (
        <div style={{
          flex: 1, display: 'flex', flexDirection: 'column', alignItems: 'center',
          justifyContent: 'center', color: 'var(--color-text-muted)', fontSize: '13px',
          textAlign: 'center', lineHeight: 1.8, padding: '24px', gap: '12px',
        }}>
          <div>請選擇過去對話</div>
          <div style={{ color: 'var(--color-text-muted)', fontSize: '11px' }}>或點擊左側「＋ 新對話」{sidebarOpen ? '' : '（先展開列表）'}</div>
        </div>
      )}
      {conversationId && <>
      {/* Voice Overlay */}
      {showVoiceOverlay && (
        <VoiceOverlay
          voiceState={voiceState}
          transcript={voiceTranscript}
          preview={voicePreview}
          previewEnabled={previewEnabled}
          isSpeaking={voiceIsSpeaking}
          onConfirm={handleVoiceConfirm}
          onDiscard={handleVoiceDiscard}
          onSaveToCurrentNote={currentPath ? handleVoiceSaveToCurrentNote : undefined}
          onSaveToNewNote={handleVoiceSaveToNewNote}
        />
      )}
      <div style={{ display: 'contents' }}>

        {/* 未設定警告 */}
        {!isConfigured && (
          <div style={{
            margin: '10px 12px', padding: '8px 10px', borderRadius: '6px',
            background: 'rgba(245,158,11,0.08)', border: '1px solid rgba(245,158,11,0.25)',
            fontSize: '12px', color: 'var(--color-warning, #f59e0b)', lineHeight: 1.5,
          }}>
            ⚠ 請先到 <strong>Settings &gt; AI</strong> 設定 llama CLI 路徑與本地模型。
          </div>
        )}

        {/* 接近閾值警告（未開啟自動記憶時顯示） */}
        {(() => {
          const threshold = settings.memory_threshold ?? 20
          const meaningful = messages.filter(m => m.role === 'user' || m.role === 'assistant').length
          const nearLimit = meaningful >= Math.max(threshold - 4, 1) && meaningful < threshold
          return !settings.enable_auto_memory && nearLimit ? (
            <div style={{
              margin: '4px 12px 0', padding: '6px 10px', borderRadius: '6px',
              background: 'rgba(245,158,11,0.08)', border: '1px solid rgba(245,158,11,0.25)',
              fontSize: '11px', color: 'var(--color-warning, #f59e0b)', lineHeight: 1.5,
              display: 'flex', alignItems: 'center', justifyContent: 'space-between',
            }}>
              <span>對話已達 {meaningful} 則，接近閾值 {threshold}，建議壓縮記憶。</span>
              <button
                onClick={() => compressToKnowledge(false)}
                disabled={isCompressing}
                style={{
                  marginLeft: '8px', fontSize: '11px', padding: '2px 8px', borderRadius: '4px',
                  background: 'rgba(245,158,11,0.15)', border: '1px solid rgba(245,158,11,0.4)',
                  color: 'var(--color-warning, #f59e0b)', cursor: 'pointer', flexShrink: 0,
                }}
              >立即壓縮</button>
            </div>
          ) : null
        })()}

        {/* 訊息列表 */}
        <div style={{ flex: 1, overflowY: 'auto', padding: '8px 12px', display: 'flex', flexDirection: 'column', gap: '6px' }}>
          {messages.length === 0 && !isStreaming && (
            <div style={{
              display: 'flex', alignItems: 'center', justifyContent: 'center',
              height: '100%', color: 'var(--color-text-muted)', fontSize: '12px',
              textAlign: 'center', lineHeight: 1.6,
            }}>
              {'與本地 LLM 對話（自動使用工具）'}
              <br />
              <span style={{ fontSize: '11px', opacity: 0.7 }}>按 Enter 送出，Shift+Enter 換行</span>
            </div>
          )}

          {(() => {
            let lastUserContent = ''
            return messages.map((msg, i) => {
              if (msg.role === 'user') lastUserContent = msg.content
              const capturedUser = lastUserContent
              return (
                <MessageBubble
                  key={i}
                  message={msg}
                  onSaveWeb={msg.webRefs?.length ? () => handleSaveWeb(i, msg) : undefined}
                  savingWeb={savingWebMsgIdx === i}
                  onExtractSkill={msg.role === 'assistant' && !isStreaming && capturedUser
                    ? () => doExtractSkill(capturedUser, msg.content)
                    : undefined}
                  rating={ratings[i]}
                  onRate={msg.role === 'assistant' && !isStreaming
                    ? (r: 'good' | 'bad') => {
                        setRatings(prev => ({ ...prev, [i]: r }))
                        const hash = msg.content.slice(0, 150)
                        invoke('rate_response', {
                          conversationId: conversationId ?? undefined,
                          contentHash: hash,
                          rating: r,
                        }).catch(() => {})
                      }
                    : undefined}
                />
              )
            })
          })()}

          {/* 串流中 / agent 思考中的 assistant 泡泡 */}
          {isStreaming && (
            <MessageBubble
              message={{ role: 'assistant', content: streamingText }}
              streaming
            />
          )}

          {/* 錯誤訊息 */}
          {error && (
            <div style={{
              padding: '8px 10px', borderRadius: '6px', fontSize: '12px',
              background: 'rgba(239,68,68,0.08)', border: '1px solid rgba(239,68,68,0.2)',
              color: 'var(--color-error, #ef4444)', wordBreak: 'break-all',
            }}>
              ⚠ {error}
            </div>
          )}

          <div ref={bottomRef} />
        </div>

        {/* 筆記建議捷徑 */}
        {noteSuggestions.length > 0 && onOpenNote && (
          <div style={{ padding: '4px 8px', display: 'flex', flexWrap: 'wrap', gap: '4px', flexShrink: 0 }}>
            <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', alignSelf: 'center' }}>找到相關段落，要打開嗎？</span>
            {noteSuggestions.map((note, i) => (
              <button
                key={i}
                onClick={() => { setNoteSuggestions([]); noteSuggestionsRef.current = []; onOpenNote(note.absPath) }}
                style={{
                  fontSize: '11px', padding: '2px 8px', borderRadius: '12px',
                  border: '1px solid var(--color-accent)', background: 'var(--color-accent-dim)',
                  color: 'var(--color-accent)', cursor: 'pointer',
                }}
              >
                {note.label}
              </button>
            ))}
          </div>
        )}

        {/* 搜尋方式選擇 Bubble */}
        {pendingSearchMethod && (
          <div style={{
            margin: '0 8px 8px', padding: '10px 14px', borderRadius: '8px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-accent)',
            fontSize: '13px', flexShrink: 0,
          }}>
            <div style={{ marginBottom: '6px', color: 'var(--color-text-secondary)', fontSize: '12px' }}>
              請選擇搜尋方式
            </div>
            <div style={{ marginBottom: '8px', color: 'var(--color-text-primary)', fontSize: '12px', fontFamily: 'var(--font-mono, monospace)' }}>
              查詢：{pendingSearchMethod.query}
            </div>
            <div style={{ display: 'flex', gap: '6px' }}>
              <button
                onClick={() => handleSearchMethodChoice('web_search')}
                style={{ padding: '4px 12px', borderRadius: '4px', background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer', fontSize: '12px' }}
              >網路搜尋</button>
              <button
                onClick={() => handleSearchMethodChoice('call_external_ai')}
                style={{ padding: '4px 12px', borderRadius: '4px', background: 'transparent', color: 'var(--color-text-secondary)', border: '1px solid var(--color-border)', cursor: 'pointer', fontSize: '12px' }}
              >外部 AI</button>
            </div>
          </div>
        )}

        {/* 寫入確認 Bubble */}
        {pendingWriteDisplay && (
          <div style={{
            margin: '0 8px 8px', padding: '10px 14px', borderRadius: '8px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-accent)',
            fontSize: '13px', flexShrink: 0,
          }}>
            <div style={{ marginBottom: '8px', color: 'var(--color-text-primary)', whiteSpace: 'pre-wrap', fontFamily: 'var(--font-mono, monospace)', fontSize: '12px' }}>
              {pendingWriteDisplay}
            </div>
            <div style={{ fontSize: '12px', color: 'var(--color-text-secondary)', marginBottom: '8px' }}>
              LLM 想執行此寫入操作，是否允許？
            </div>
            <div style={{ display: 'flex', gap: '6px' }}>
              <button
                onClick={() => handleWriteConfirm(true)}
                style={{ padding: '4px 12px', borderRadius: '4px', background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer', fontSize: '12px' }}
              >允許</button>
              <button
                onClick={() => handleWriteConfirm(false)}
                style={{ padding: '4px 12px', borderRadius: '4px', background: 'transparent', color: 'var(--color-text-secondary)', border: '1px solid var(--color-border)', cursor: 'pointer', fontSize: '12px' }}
              >拒絕</button>
            </div>
          </div>
        )}

        {/* 技能預覽卡片 */}
        {skillPreview && (
          <div style={{
            margin: '0 8px 8px', padding: '12px 14px', borderRadius: '8px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-accent)',
            fontSize: '12px', flexShrink: 0,
          }}>
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: '8px' }}>
              <span style={{ fontWeight: 600, color: 'var(--color-accent)', fontSize: '12px' }}>📌 建議技能規範</span>
              <button onClick={() => setSkillPreview(null)} style={{ background: 'none', border: 'none', color: 'var(--color-text-muted)', cursor: 'pointer', fontSize: '14px', lineHeight: 1 }}>✕</button>
            </div>
            {skillPreview.loading ? (
              <div style={{ color: 'var(--color-text-muted)', fontSize: '12px', padding: '4px 0' }}>萃取中…</div>
            ) : (
              <>
                <div style={{ display: 'flex', flexDirection: 'column', gap: '6px' }}>
                  <label style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>標題</label>
                  <input
                    value={skillPreview.title}
                    onChange={e => setSkillPreview(prev => prev ? { ...prev, title: e.target.value } : null)}
                    style={{ padding: '4px 8px', borderRadius: '4px', border: '1px solid var(--color-border)', background: 'var(--color-bg-overlay)', color: 'var(--color-text-primary)', fontSize: '12px' }}
                  />
                  <label style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>觸發條件</label>
                  <textarea
                    value={skillPreview.trigger}
                    onChange={e => setSkillPreview(prev => prev ? { ...prev, trigger: e.target.value } : null)}
                    rows={2}
                    style={{ padding: '4px 8px', borderRadius: '4px', border: '1px solid var(--color-border)', background: 'var(--color-bg-overlay)', color: 'var(--color-text-primary)', fontSize: '12px', resize: 'vertical', fontFamily: 'var(--font-sans)' }}
                  />
                  <label style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>行為指令</label>
                  <textarea
                    value={skillPreview.behavior}
                    onChange={e => setSkillPreview(prev => prev ? { ...prev, behavior: e.target.value } : null)}
                    rows={3}
                    style={{ padding: '4px 8px', borderRadius: '4px', border: '1px solid var(--color-border)', background: 'var(--color-bg-overlay)', color: 'var(--color-text-primary)', fontSize: '12px', resize: 'vertical', fontFamily: 'var(--font-sans)' }}
                  />
                  <label style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>觸發時機</label>
                  <div style={{ display: 'flex', flexDirection: 'column', gap: '4px' }}>
                    {(['passive', 'active'] as const).map(mode => (
                      <label key={mode} style={{ display: 'flex', alignItems: 'center', gap: '6px', fontSize: '11px', cursor: 'pointer' }}>
                        <input
                          type="radio"
                          name="chat-injection-mode"
                          checked={skillPreview.injectionMode === mode}
                          onChange={() => setSkillPreview(prev => prev ? { ...prev, injectionMode: mode } : null)}
                        />
                        {mode === 'passive' ? '被動取用（相似度比對）' : '主動注入（每次對話）'}
                      </label>
                    ))}
                  </div>
                </div>
                <div style={{ display: 'flex', gap: '6px', marginTop: '10px' }}>
                  <button
                    onClick={confirmSkill}
                    disabled={savingSkill}
                    style={{ padding: '4px 14px', borderRadius: '4px', background: 'var(--color-accent)', color: '#fff', border: 'none', cursor: 'pointer', fontSize: '12px', opacity: savingSkill ? 0.6 : 1 }}
                  >{savingSkill ? '建立中…' : '確認建立'}</button>
                  <button
                    onClick={() => setSkillPreview(null)}
                    style={{ padding: '4px 12px', borderRadius: '4px', background: 'transparent', color: 'var(--color-text-secondary)', border: '1px solid var(--color-border)', cursor: 'pointer', fontSize: '12px' }}
                  >取消</button>
                </div>
              </>
            )}
          </div>
        )}

        {/* 輸入區 */}
        <div style={{
          padding: '8px 12px', borderTop: '1px solid var(--color-border)', flexShrink: 0,
          display: 'flex', gap: '6px', alignItems: 'flex-end',
        }}>
          <textarea
            ref={inputRef}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={handleKeyDown}
            disabled={isStreaming || !isConfigured}
            placeholder={isConfigured ? '輸入訊息…' : '請先設定 llama CLI'}
            rows={1}
            style={{
              flex: 1, resize: 'none', padding: '7px 10px',
              background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
              borderRadius: '6px', color: 'var(--color-text-primary)', fontSize: '13px',
              outline: 'none', fontFamily: 'var(--font-sans)', lineHeight: 1.5,
              minHeight: '34px', maxHeight: '120px', overflowY: 'auto',
              opacity: !isConfigured ? 0.5 : 1,
            }}
            onInput={(e) => {
              const t = e.currentTarget
              t.style.height = 'auto'
              t.style.height = Math.min(t.scrollHeight, 120) + 'px'
            }}
          />
          {/* 錄音按鈕 */}
          <button
            onClick={toggleVoice}
            disabled={!whisperConfigured || voiceState === 'transcribing' || showVoiceOverlay || liveChatActive}
            title={!whisperConfigured ? '請先到設定頁設定 Whisper' : voiceState === 'recording' ? '停止錄音' : '語音輸入'}
            style={{
              width: '34px', height: '34px', borderRadius: '6px', flexShrink: 0,
              display: 'flex', alignItems: 'center', justifyContent: 'center',
              border: '1px solid var(--color-border)',
              background: voiceState === 'recording'
                ? 'rgba(239,68,68,0.15)'
                : voiceState === 'done'
                  ? 'rgba(74,222,128,0.15)'
                  : 'var(--color-bg-elevated)',
              color: voiceState === 'recording'
                ? '#ef4444'
                : voiceState === 'done'
                  ? '#4ade80'
                  : voiceState === 'error'
                    ? '#f59e0b'
                    : 'var(--color-text-muted)',
              cursor: !whisperConfigured || voiceState === 'transcribing' ? 'not-allowed' : 'pointer',
              opacity: !whisperConfigured || voiceState === 'transcribing' ? 0.4 : 1,
              transition: 'background 0.2s, color 0.2s',
            }}
          >
            {voiceState === 'recording' ? (
              // 停止方塊
              <svg width="11" height="11" viewBox="0 0 24 24" fill="currentColor">
                <rect x="3" y="3" width="18" height="18" rx="2"/>
              </svg>
            ) : voiceState === 'transcribing' ? (
              // 轉圈點點
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round">
                <circle cx="12" cy="12" r="9" strokeDasharray="28 56" strokeDashoffset="0">
                  <animateTransform attributeName="transform" type="rotate" from="0 12 12" to="360 12 12" dur="0.8s" repeatCount="indefinite"/>
                </circle>
              </svg>
            ) : (
              // 麥克風
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                <path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z"/>
                <path d="M19 10v2a7 7 0 0 1-14 0v-2"/>
                <line x1="12" y1="19" x2="12" y2="23"/>
                <line x1="8" y1="23" x2="16" y2="23"/>
              </svg>
            )}
          </button>
          {/* 送出按鈕（箭頭） */}
          <button
            onClick={send}
            disabled={isStreaming || !input.trim() || !isConfigured}
            title="送出"
            style={{
              width: '34px', height: '34px', borderRadius: '6px', flexShrink: 0,
              display: 'flex', alignItems: 'center', justifyContent: 'center',
              background: isStreaming || !input.trim() || !isConfigured ? 'var(--color-bg-overlay)' : 'var(--color-accent)',
              color: isStreaming || !input.trim() || !isConfigured ? 'var(--color-text-muted)' : 'white',
              border: 'none',
              cursor: isStreaming || !input.trim() || !isConfigured ? 'not-allowed' : 'pointer',
              opacity: isStreaming || !input.trim() || !isConfigured ? 0.5 : 1,
            }}
          >
            {isStreaming ? (
              <span style={{ fontSize: '16px', lineHeight: 1 }}>…</span>
            ) : (
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <line x1="12" y1="19" x2="12" y2="5"/>
                <polyline points="5 12 12 5 19 12"/>
              </svg>
            )}
          </button>
        </div>
      </div>
      </>}{/* end conversationId guard */}
      </div>{/* end main chat area */}
    </div>
  )
}


function MessageBubble({
  message,
  streaming,
  onSaveWeb,
  savingWeb,
  onExtractSkill,
  rating,
  onRate,
}: {
  message: Message
  streaming?: boolean
  onSaveWeb?: () => void
  savingWeb?: boolean
  onExtractSkill?: () => void
  rating?: 'good' | 'bad'
  onRate?: (r: 'good' | 'bad') => void
}) {
  const isUser = message.role === 'user'
  const isTool = message.role === 'tool'
  const isNotice = message.role === 'notice'

  if (isNotice) {
    return (
      <div style={{
        textAlign: 'center', fontSize: '11px', color: 'var(--color-text-muted)',
        padding: '4px 0', opacity: 0.7,
      }}>
        ── {message.content} ──
      </div>
    )
  }

  if (isTool) {
    return (
      <div style={{ display: 'flex', justifyContent: 'flex-start' }}>
        <div style={{
          maxWidth: '92%', padding: '4px 10px', borderRadius: '6px',
          background: 'var(--color-bg-overlay)',
          color: 'var(--color-text-muted)',
          fontSize: '11px', lineHeight: 1.5,
          fontFamily: 'var(--font-mono, monospace)',
          border: '1px solid var(--color-border)',
          whiteSpace: 'pre-wrap',
        }}>
          {message.content}
        </div>
      </div>
    )
  }

  return (
    <div style={{ display: 'flex', justifyContent: isUser ? 'flex-end' : 'flex-start' }}>
      <div style={{ maxWidth: '85%', display: 'flex', flexDirection: 'column', alignItems: isUser ? 'flex-end' : 'flex-start' }}>
        <div style={{
          padding: '8px 12px',
          borderRadius: isUser ? '12px 12px 4px 12px' : '12px 12px 12px 4px',
          background: isUser ? 'var(--color-accent)' : 'var(--color-bg-elevated)',
          color: isUser ? 'white' : 'var(--color-text-primary)',
          fontSize: '13px', lineHeight: 1.6, wordBreak: 'break-word',
          border: isUser ? 'none' : '1px solid var(--color-border)',
          whiteSpace: 'pre-wrap',
        }}>
          {message.content}
          {streaming && (
            <span style={{
              display: 'inline-block', width: '2px', height: '14px',
              background: 'var(--color-accent)', marginLeft: '2px', verticalAlign: 'text-bottom',
              animation: 'blink 1s step-start infinite',
            }} />
          )}
          {streaming && (
            <style>{`@keyframes blink { 0%, 100% { opacity: 1 } 50% { opacity: 0 } }`}</style>
          )}
        </div>
        {!isUser && !isTool && !isNotice && (onSaveWeb || onExtractSkill || onRate) && (
          <div style={{ display: 'flex', gap: '4px', marginTop: '2px', alignItems: 'center' }}>
            {onSaveWeb && (
              <button
                className={message.savedWeb ? 'import-panel-v2__saved-btn' : 'import-panel-v2__save-btn'}
                onClick={message.savedWeb ? undefined : onSaveWeb}
                disabled={savingWeb || message.savedWeb}
              >
                {message.savedWeb ? '✓ 已儲存' : savingWeb ? '儲存中…' : '儲存為知識'}
              </button>
            )}
            {onExtractSkill && (
              <button
                onClick={onExtractSkill}
                title="從此回覆萃取技能規範"
                style={{
                  fontSize: '11px', padding: '2px 8px', borderRadius: '10px',
                  background: 'transparent', border: '1px solid var(--color-border)',
                  color: 'var(--color-text-muted)', cursor: 'pointer',
                }}
              >📌 轉為技能規範</button>
            )}
            {onRate && (
              <>
                <button
                  onClick={() => !rating && onRate('good')}
                  title="這個回覆有幫助"
                  style={{
                    fontSize: '11px', padding: '2px 6px', borderRadius: '10px',
                    background: rating === 'good' ? 'rgba(48,209,88,0.15)' : 'transparent',
                    border: `1px solid ${rating === 'good' ? 'rgba(48,209,88,0.5)' : 'var(--color-border)'}`,
                    color: rating === 'good' ? '#30d158' : 'var(--color-text-muted)',
                    cursor: rating ? 'default' : 'pointer',
                  }}
                >👍</button>
                <button
                  onClick={() => !rating && onRate('bad')}
                  title="這個回覆不夠好"
                  style={{
                    fontSize: '11px', padding: '2px 6px', borderRadius: '10px',
                    background: rating === 'bad' ? 'rgba(255,69,58,0.15)' : 'transparent',
                    border: `1px solid ${rating === 'bad' ? 'rgba(255,69,58,0.5)' : 'var(--color-border)'}`,
                    color: rating === 'bad' ? '#ff453a' : 'var(--color-text-muted)',
                    cursor: rating ? 'default' : 'pointer',
                  }}
                >👎</button>
              </>
            )}
          </div>
        )}
      </div>
    </div>
  )
}
