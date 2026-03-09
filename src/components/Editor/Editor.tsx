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
import { wikilinkPlugin } from './plugins/wikilinks'
import Toolbar, { EditorAction } from './Toolbar'
import PreviewPanel from './PreviewPanel'
import BacklinksPanel from '../BacklinksPanel/BacklinksPanel'
import { toast } from '../common/Toast'

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
  onOpenNote: (path: string) => void
}

export default function Editor({ canGoBack, canGoForward, onBack, onForward, onOpenNote }: EditorProps) {
  const editorRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Ref so the CM6 updateListener always calls the latest version (avoids stale closure)
  const triggerAutoSaveRef = useRef<(content: string) => void>(() => {})

  const { currentPath, content, isDirty, viewMode, setContent, setDirty, setViewMode,
          pendingContent, clearPendingContent } = useEditorStore()
  const [pendingAnchor, setPendingAnchor] = useState<string | undefined>(undefined)
  const { readNote, updateNote } = useVaultStore()
  const { settings } = useSettingsStore()

  const openNote = useCallback((path: string) => {
    onOpenNote(path)
    setPendingAnchor(undefined)  // 清除殘留 anchor（非錨點導航時）
  }, [onOpenNote])

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

  // 外部寫入同步：ChatPanel 等外部呼叫 applyExternalWrite 後，把新內容同步到 CM6 視圖
  useEffect(() => {
    if (pendingContent === null) return
    const view = viewRef.current
    if (view) {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: pendingContent },
      })
      // CM6 onChange 會呼叫 setContent 並把 isDirty 設為 true，
      // 這裡馬上清回 false，避免 auto-save 以「舊+新」內容再蓋一次
      setDirty(false)
    }
    clearPendingContent()
  }, [pendingContent, clearPendingContent, setDirty])


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

  const handleTextStyle = useCallback((text: string, styleStr: string, contextBefore: string) => {
    const view = viewRef.current
    if (!view) return
    const doc = view.state.doc.toString()
    // Collect all occurrences of the selected text
    const occurrences: number[] = []
    let search = 0
    while (true) {
      const i = doc.indexOf(text, search)
      if (i < 0) break
      occurrences.push(i)
      search = i + 1
    }
    if (occurrences.length === 0) return
    // Strip HTML tags so docBefore (which may contain <span> etc.) compares correctly
    // against contextBefore (which is plain rendered text)
    const stripTags = (s: string) => s.replace(/<[^>]*>/g, '')
    // Pick occurrence whose preceding plain-text best matches contextBefore (longest common suffix)
    let bestIdx = occurrences[0]
    let bestScore = -1
    for (const idx of occurrences) {
      // Use a larger window (× 4) to account for HTML tag overhead after stripping
      const rawBefore = doc.slice(Math.max(0, idx - (contextBefore.length + 20) * 4), idx)
      const docBefore = stripTags(rawBefore)
      let score = 0
      for (let k = 1; k <= Math.min(contextBefore.length, docBefore.length); k++) {
        if (contextBefore[contextBefore.length - k] === docBefore[docBefore.length - k]) score++
        else break
      }
      if (score > bestScore) { bestScore = score; bestIdx = idx }
    }
    view.dispatch({
      changes: { from: bestIdx, to: bestIdx + text.length, insert: `<span style="${styleStr}">${text}</span>` },
    })
  }, [])

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
            onTextStyle={handleTextStyle}
          />
        )}
      </div>

      <BacklinksPanel onOpenNote={openNote} />
    </div>
  )
}
