import { useEffect, useRef, useCallback, useState } from 'react'
import { EditorView, basicSetup } from 'codemirror'
import { EditorState } from '@codemirror/state'
import { markdown, markdownLanguage } from '@codemirror/lang-markdown'
import { languages } from '@codemirror/language-data'
import { keymap } from '@codemirror/view'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { open as openDialog } from '@tauri-apps/plugin-dialog'
import { useEditorStore } from '../../stores/editorStore'
import { useVaultStore } from '../../stores/vaultStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { useNavigationStore } from '../../stores/navigationStore'
import { wikilinkPlugin } from './plugins/wikilinks'
import Toolbar, { EditorAction } from './Toolbar'
import PreviewPanel from './PreviewPanel'
import BacklinksPanel from '../BacklinksPanel/BacklinksPanel'
import { toast } from '../common/Toast'
import { invoke } from '@tauri-apps/api/core'
import { useVoiceRecorder } from '../../hooks/useVoiceRecorder'

// CSS variables are live — when data-theme changes the editor repaints automatically
const editorTheme = EditorView.theme({
  '&': { background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', height: '100%' },
  '.cm-content': { padding: '24px 32px', fontFamily: 'var(--font-mono)', fontSize: 'var(--font-size-editor)', lineHeight: '1.7', caretColor: 'var(--color-accent)', WebkitUserSelect: 'text' },
  '.cm-gutters': { background: 'var(--color-bg-base)', borderRight: '1px solid var(--color-border)', color: 'var(--color-text-muted)' },
  '.cm-activeLine': { background: 'var(--color-accent-dim)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground': { background: 'rgba(124,140,248,0.4)' },
  '.cm-cursor': { borderLeftColor: 'var(--color-accent)' },
  '.cm-wikilink-mark': { color: 'var(--color-accent)', textDecoration: 'underline dotted' },
})

interface EditorProps {
  canGoBack: boolean
  canGoForward: boolean
  onBack: () => void
  onForward: () => void
}

export default function Editor({ canGoBack, canGoForward, onBack, onForward }: EditorProps) {
  const editorRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Ref so the CM6 updateListener always calls the latest version (avoids stale closure)
  const triggerAutoSaveRef = useRef<(content: string) => void>(() => {})

  const { currentPath, content, isDirty, viewMode, setContent, setDirty, setCurrentPath, setViewMode } = useEditorStore()
  const [pendingAnchor, setPendingAnchor] = useState<string | undefined>(undefined)
  const { readNote, updateNote } = useVaultStore()
  const { settings } = useSettingsStore()
  const { push: navPush } = useNavigationStore()

  const openNote = useCallback((path: string) => {
    navPush(path)
    setCurrentPath(path)
    setPendingAnchor(undefined)  // 清除殘留 anchor（非錨點導航時）
  }, [navPush, setCurrentPath])

  // 載入筆記（帶 cancellation 防止快速切換時 race condition）
  useEffect(() => {
    if (!currentPath) {
      // 筆記關閉或刪除時清空 CM6 編輯器
      if (viewRef.current) {
        viewRef.current.dispatch({
          changes: { from: 0, to: viewRef.current.state.doc.length, insert: '' }
        })
      }
      return
    }
    let cancelled = false
    readNote(currentPath).then((note) => {
      if (cancelled) return
      if (viewRef.current) {
        viewRef.current.dispatch({
          changes: { from: 0, to: viewRef.current.state.doc.length, insert: note.content }
        })
      }
      setContent(note.content)
      setDirty(false)
    }).catch((e) => {
      if (!cancelled) toast.error('讀取失敗：' + e.message)
    })
    return () => { cancelled = true }
  }, [currentPath])

  // 初始化 CM6（只跑一次；用 ref 橋接 triggerAutoSave 避免 stale closure）
  useEffect(() => {
    if (!editorRef.current) return

    const view = new EditorView({
      state: EditorState.create({
        doc: '',
        extensions: [
          basicSetup,
          history(),
          markdown({ base: markdownLanguage, codeLanguages: languages }),
          editorTheme,
          wikilinkPlugin,
          keymap.of([...defaultKeymap, ...historyKeymap]),
          EditorView.updateListener.of((update) => {
            if (update.docChanged) {
              const newContent = update.state.doc.toString()
              setContent(newContent)
              triggerAutoSaveRef.current(newContent)
            }
          }),
        ],
      }),
      parent: editorRef.current,
    })

    viewRef.current = view
    return () => { view.destroy(); viewRef.current = null }
  }, [])

  const triggerAutoSave = useCallback((newContent: string) => {
    if (settings.auto_save_mode !== 'afterDelay') return
    if (saveTimerRef.current) clearTimeout(saveTimerRef.current)
    saveTimerRef.current = setTimeout(async () => {
      if (!currentPath) return
      try {
        await updateNote(currentPath, newContent)
        setDirty(false)
      } catch {}
    }, settings.auto_save_delay)
  }, [currentPath, settings.auto_save_mode, settings.auto_save_delay])

  // 每次 triggerAutoSave 更新時同步給 ref
  useEffect(() => {
    triggerAutoSaveRef.current = triggerAutoSave
  }, [triggerAutoSave])

  const save = useCallback(async () => {
    if (!currentPath || !isDirty) return
    try {
      await updateNote(currentPath, content)
      setDirty(false)
    } catch (e: any) {
      toast.error('儲存失敗：' + e.message)
    }
  }, [currentPath, content, isDirty])

  // Cmd+S 儲存
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === 's') { e.preventDefault(); save() }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [save])

  // 語音輸入：辨識完成後插入游標處（或 toast 通知）
  const insertAtCursor = useCallback((text: string) => {
    const view = viewRef.current
    if (!view || !currentPath) {
      toast.success('語音轉錄：' + text)
      return
    }
    const cursor = view.state.selection.main.head
    const insert = cursor > 0 ? ' ' + text : text
    view.dispatch({
      changes: { from: cursor, insert },
      selection: { anchor: cursor + insert.length },
    })
    view.focus()
  }, [currentPath])

  const handleTranscript = useCallback(async (text: string) => {
    const mode = settings.voice_process_mode ?? 'none'

    // 有設定後處理模式且 llama 已設定
    if (mode !== 'none' && settings.llama_cli_path) {
      // system 放角色指令，userContent 放待處理文字
      // 分開傳讓模型知道這是任務而非對話，避免模型回應「請提供文字」
      const system = mode === 'format'
        ? '你是文字整理助手。你的唯一任務是將輸入的口語錄音文字整理成通順的書面文字，保留原意，不要增加任何新內容。直接輸出整理後的文字，不要問問題、不要說明、不要對話。'
        : '你是筆記助手。你的唯一任務是為輸入的內容用繁體中文生成 YAML frontmatter，包含 title、tags（陣列）、summary 三個欄位。只輸出 frontmatter，用 --- 包圍，不要問問題、不要說明、不要對話。'
      toast.info('llama 處理中…')
      try {
        const result = await invoke<string>('process_with_llm', { system, userContent: text })
        insertAtCursor(result.trim())
      } catch (e: any) {
        const msg = e?.AI ?? (typeof e === 'string' ? e : JSON.stringify(e))
        toast.error('llama 處理失敗：' + msg)
        // fallback：插入原始轉錄文字
        if (settings.whisper_auto_insert) insertAtCursor(text)
      }
      return
    }

    // 預設行為
    if (settings.whisper_auto_insert) {
      insertAtCursor(text)
    } else {
      toast.success('轉錄完成（點擊複製）：' + text.slice(0, 60) + (text.length > 60 ? '…' : ''))
    }
  }, [settings.voice_process_mode, settings.llama_cli_path, settings.whisper_auto_insert, insertAtCursor])

  const whisperConfigured = !!settings.whisper_model_path
  const { state: voiceState, errorMsg: voiceError, segmentsDone: voiceSegmentsDone, toggle: voiceToggle } =
    useVoiceRecorder(handleTranscript)

  const handleAction = useCallback(async (action: EditorAction) => {
    const view = viewRef.current
    if (!view) return
    const { from, to } = view.state.selection.main
    const selected = view.state.doc.sliceString(from, to)

    switch (action) {
      case 'bold': {
        if (selected) {
          view.dispatch({
            changes: { from, to, insert: `**${selected}**` },
            selection: { anchor: from + selected.length + 4 },
          })
        } else {
          view.dispatch({
            changes: { from, to, insert: '****' },
            selection: { anchor: from + 2 },
          })
        }
        break
      }
      case 'italic': {
        if (selected) {
          view.dispatch({
            changes: { from, to, insert: `*${selected}*` },
            selection: { anchor: from + selected.length + 2 },
          })
        } else {
          view.dispatch({
            changes: { from, to, insert: '**' },
            selection: { anchor: from + 1 },
          })
        }
        break
      }
      case 'h1':
      case 'h2': {
        const prefix = action === 'h1' ? '# ' : '## '
        const line = view.state.doc.lineAt(from)
        const stripped = line.text.replace(/^#{1,6}\s+/, '')
        if (line.text.startsWith(prefix)) {
          // Toggle off：移除 heading
          view.dispatch({
            changes: { from: line.from, to: line.to, insert: stripped },
            selection: { anchor: line.from + stripped.length },
          })
        } else {
          // 套用（或替換現有 heading）
          view.dispatch({
            changes: { from: line.from, to: line.to, insert: prefix + stripped },
            selection: { anchor: line.from + prefix.length + stripped.length },
          })
        }
        break
      }
      case 'wikilink': {
        view.dispatch({
          changes: { from, to, insert: '[[]]' },
          selection: { anchor: from + 2 },
        })
        break
      }
      case 'image': {
        try {
          const file = await openDialog({
            multiple: false,
            filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg'] }],
          })
          if (!file) break
          const filePath = typeof file === 'string' ? file : (file as any).path ?? String(file)
          const filename = filePath.split('/').pop() ?? 'image'
          view.dispatch({
            changes: { from, to, insert: `![${filename}](${filePath})` },
            selection: { anchor: from + 2 },
          })
        } catch {}
        break
      }
    }
    view.focus()
  }, [])

  const showEditor = viewMode === 'split' || viewMode === 'editor'
  const showPreviewPane = viewMode === 'preview' || viewMode === 'split'

  const wikilinkHandler = (title: string, anchor?: string) => {
    const note = useVaultStore.getState().notes.find((n) => n.title === title)
    if (!note) return
    if (note.path === currentPath) {
      setPendingAnchor(anchor)
      return
    }
    openNote(note.path)
    setPendingAnchor(anchor)
  }

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%' }}>
      {/* Placeholder when no note is open */}
      {!currentPath && (
        <div style={{
          position: 'absolute', inset: 0, display: 'flex',
          alignItems: 'center', justifyContent: 'center',
          color: 'var(--color-text-muted)', flexDirection: 'column', gap: '8px',
          pointerEvents: 'none', zIndex: 1,
        }}>
          <p style={{ fontSize: '16px' }}>選擇或建立一篇筆記</p>
          <p style={{ fontSize: '13px' }}>按 ⌘P 快速開啟</p>
        </div>
      )}

      {/* Toolbar：preview 模式下只保留導覽，其餘模式顯示完整工具列 */}
      <Toolbar
        canGoBack={canGoBack}
        canGoForward={canGoForward}
        onBack={onBack}
        onForward={onForward}
        onAction={handleAction}
        onSave={save}
        voiceState={whisperConfigured ? voiceState : undefined}
        voiceError={voiceError}
        voiceSegmentsDone={voiceSegmentsDone}
        onVoiceToggle={whisperConfigured ? voiceToggle : undefined}
      />

      <div style={{ flex: 1, display: 'flex', overflow: 'hidden', position: 'relative' }}>
        {/* CM6 Editor — always mounted; hidden via display:none in preview mode */}
        <div
          ref={editorRef}
          style={{
            display: showEditor ? 'flex' : 'none',
            flex: viewMode === 'split' ? '0 0 50%' : '1',
            overflow: 'hidden',
            borderRight: viewMode === 'split' ? '1px solid var(--color-border)' : 'none',
          }}
        />

        {/* Preview Panel */}
        {showPreviewPane && currentPath && (
          <PreviewPanel
            content={content}
            onWikilinkClick={wikilinkHandler}
            onEdit={viewMode === 'preview' ? () => setViewMode('split') : undefined}
            pendingAnchor={pendingAnchor}
            onAnchorScrolled={() => setPendingAnchor(undefined)}
          />
        )}
      </div>

      <BacklinksPanel onOpenNote={openNote} />
    </div>
  )
}
