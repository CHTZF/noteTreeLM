import { useEffect, useRef, useCallback, useState } from 'react'
import { EditorView, basicSetup } from 'codemirror'
import { EditorState, Compartment } from '@codemirror/state'
import { markdown, markdownLanguage } from '@codemirror/lang-markdown'
import { languages } from '@codemirror/language-data'
import { keymap } from '@codemirror/view'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { open as openDialog } from '@tauri-apps/plugin-dialog'
import { useEditorStore } from '../../stores/editorStore'
import { useVaultStore } from '../../stores/vaultStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { wikilinkPlugin } from './plugins/wikilinks'
import { livePreviewPlugin, livePreviewTheme } from './plugins/livePreview'
import type { EditorAction } from './Toolbar'
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
  canGoBack?: boolean
  canGoForward?: boolean
  onBack?: () => void
  onForward?: () => void
  onOpenNote: (path: string) => void
}

export default function Editor({ onOpenNote }: EditorProps) {
  const editorRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Ref so the CM6 updateListener always calls the latest version (avoids stale closure)
  const triggerAutoSaveRef = useRef<(content: string) => void>(() => {})
  // Compartment to toggle live preview extension dynamically
  const liveCompartment = useRef(new Compartment())

  const { currentPath, content, isDirty, viewMode, setContent, setDirty, setViewMode,
          pendingContent, clearPendingContent } = useEditorStore()
  const [pendingAnchor, setPendingAnchor] = useState<string | undefined>(undefined)
  const { readNote, updateNote } = useVaultStore()
  const { settings } = useSettingsStore()

  // ── Quick-copy modal state ────────────────────────────────────────────────
  const [quickCopyModal, setQuickCopyModal] = useState<{ from: number; to: number } | null>(null)
  const [qcDisplayText, setQcDisplayText] = useState('')
  const [qcColor, setQcColor] = useState('#2080e0')
  const [qcFontSize, setQcFontSize] = useState('')
  const [qcFontFamily, setQcFontFamily] = useState('inherit')
  const [qcFontWeight, setQcFontWeight] = useState('inherit')
  const [qcCopyContent, setQcCopyContent] = useState('')

  // ── Image modal state ─────────────────────────────────────────────────────
  const [imageModal, setImageModal] = useState<{ from: number; to: number } | null>(null)
  const [imgSource, setImgSource] = useState<'file' | 'url'>('file')
  const [imgFilePath, setImgFilePath] = useState('')
  const [imgUrl, setImgUrl] = useState('')
  const [imgSize, setImgSize] = useState('')
  const [imgAlt, setImgAlt] = useState('')
  const [imgUrlError, setImgUrlError] = useState('')

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
          livePreviewTheme,
          wikilinkPlugin,
          liveCompartment.current.of([]), // initially no live preview
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

  // Toggle live preview plugin when viewMode changes
  useEffect(() => {
    const view = viewRef.current
    if (!view) return
    try {
      view.dispatch({
        effects: liveCompartment.current.reconfigure(
          viewMode === 'live' ? [livePreviewPlugin] : []
        ),
      })
    } catch (e) {
      console.error('livePreview reconfigure error:', e)
    }
  }, [viewMode])

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
        setImgSource('file')
        setImgFilePath('')
        setImgUrl('')
        setImgSize('')
        setImgAlt(selected)
        setImgUrlError('')
        setImageModal({ from, to })
        return
      }
      case 'quick_copy': {
        setQcDisplayText(selected || '')
        setQcCopyContent(selected || '')
        setQcColor('#2080e0')
        setQcFontSize('')
        setQcFontFamily('inherit')
        setQcFontWeight('inherit')
        setQuickCopyModal({ from, to })
        return // modal takes over; don't call view.focus() yet
      }
    }
    view.focus()
  }, [])

  // ── CM6 右鍵選單 ────────────────────────────────────────────────────────────
  const [cmCtxMenu, setCmCtxMenu] = useState<{ x: number; y: number } | null>(null)
  const cmMenuRef = useRef<HTMLDivElement>(null)
  const [cmSubMenuOpen, setCmSubMenuOpen] = useState(false)
  const subMenuCloseTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const openSubMenu = () => { if (subMenuCloseTimer.current) clearTimeout(subMenuCloseTimer.current); setCmSubMenuOpen(true) }
  const closeSubMenu = () => { subMenuCloseTimer.current = setTimeout(() => setCmSubMenuOpen(false), 120) }

  useEffect(() => {
    if (!cmCtxMenu) return
    const close = () => setCmCtxMenu(null)
    setTimeout(() => window.addEventListener('mousedown', close), 0)
    return () => window.removeEventListener('mousedown', close)
  }, [cmCtxMenu])

  const handleEditQuickCopy = useCallback((dataCopyDecoded: string, newHtml: string) => {
    const view = viewRef.current
    if (!view) return
    const doc = view.state.doc.toString()
    const escaped = dataCopyDecoded.replace(/&/g, '&amp;').replace(/"/g, '&quot;')
    const escapedForRegex = escaped.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
    const regex = new RegExp(`<span[^>]+data-copy="${escapedForRegex}"[^>]*>[\\s\\S]*?<\\/span>`)
    const match = regex.exec(doc)
    if (!match) return
    view.dispatch({
      changes: { from: match.index, to: match.index + match[0].length, insert: newHtml },
    })
  }, [])

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

  const confirmQuickCopy = useCallback(() => {
    if (!quickCopyModal) return
    const view = viewRef.current
    if (!view) return
    const parts: string[] = []
    if (qcColor) parts.push(`color:${qcColor}`)
    if (qcFontSize) parts.push(`font-size:${qcFontSize}px`)
    if (qcFontFamily && qcFontFamily !== 'inherit') parts.push(`font-family:${qcFontFamily}`)
    if (qcFontWeight && qcFontWeight !== 'inherit') parts.push(`font-weight:${qcFontWeight}`)
    const styleAttr = parts.length ? ` style="${parts.join(';')}"` : ''
    const escapedCopy = qcCopyContent.replace(/&/g, '&amp;').replace(/"/g, '&quot;')
    const displayText = qcDisplayText || qcCopyContent
    const html = `<span class="quick-copy" data-copy="${escapedCopy}"${styleAttr}>${displayText}</span>`
    view.dispatch({
      changes: { from: quickCopyModal.from, to: quickCopyModal.to, insert: html },
      selection: { anchor: quickCopyModal.from + html.length },
    })
    setQuickCopyModal(null)
    view.focus()
  }, [quickCopyModal, qcDisplayText, qcColor, qcFontSize, qcFontFamily, qcFontWeight, qcCopyContent])

  const pickImageFile = useCallback(async () => {
    try {
      const file = await openDialog({
        multiple: false,
        filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp'] }],
      })
      if (!file) return
      const path = typeof file === 'string' ? file : (file as any).path ?? String(file)
      setImgFilePath(path)
      setImgAlt(prev => prev || (path.split('/').pop()?.replace(/\.[^.]+$/, '') ?? ''))
    } catch {}
  }, [])

  const confirmImageInsert = useCallback(() => {
    if (!imageModal) return
    const view = viewRef.current
    if (!view) return
    const src = (imgSource === 'file' ? imgFilePath : imgUrl).trim()
    if (!src) return
    const sizeStr = imgSize.trim() ? `|${imgSize.trim()}` : ''
    const alt = imgAlt.trim() || (imgSource === 'file'
      ? (imgFilePath.split('/').pop()?.replace(/\.[^.]+$/, '') ?? 'image')
      : 'image')
    const insert = `![${alt}${sizeStr}](${src})`
    view.dispatch({
      changes: { from: imageModal.from, to: imageModal.to, insert },
      selection: { anchor: imageModal.from + insert.length },
    })
    setImageModal(null)
    view.focus()
  }, [imageModal, imgSource, imgFilePath, imgUrl, imgAlt, imgSize])

  const applyCmAction = useCallback((action: string) => {
    setCmCtxMenu(null)
    const view = viewRef.current
    if (!view) return
    const { from, to } = view.state.selection.main
    const sel = view.state.doc.sliceString(from, to)

    // Delegate existing actions
    if (['bold','italic','wikilink','image','quick_copy'].includes(action)) {
      handleAction(action as EditorAction)
      return
    }

    const wrap = (pre: string, suf: string) => {
      view.dispatch(sel
        ? { changes: { from, to, insert: `${pre}${sel}${suf}` }, selection: { anchor: from + pre.length + sel.length + suf.length } }
        : { changes: { from, to, insert: `${pre}${suf}` }, selection: { anchor: from + pre.length } }
      )
      view.focus()
    }

    const prefixLine = (prefix: string) => {
      const line = view.state.doc.lineAt(from)
      if (line.text.startsWith(prefix)) {
        const stripped = line.text.slice(prefix.length)
        view.dispatch({ changes: { from: line.from, to: line.to, insert: stripped }, selection: { anchor: line.from + stripped.length } })
      } else {
        view.dispatch({ changes: { from: line.from, to: line.from, insert: prefix }, selection: { anchor: line.from + prefix.length + line.text.length } })
      }
      view.focus()
    }

    switch (action) {
      case 'h1': case 'h2': case 'h3': case 'h4': case 'h5': case 'h6': {
        const level = parseInt(action[1])
        const prefix = '#'.repeat(level) + ' '
        const line = view.state.doc.lineAt(from)
        const stripped = line.text.replace(/^#{1,6}\s+/, '')
        if (line.text.startsWith(prefix)) {
          view.dispatch({ changes: { from: line.from, to: line.to, insert: stripped }, selection: { anchor: line.from + stripped.length } })
        } else {
          view.dispatch({ changes: { from: line.from, to: line.to, insert: prefix + stripped }, selection: { anchor: line.from + prefix.length + stripped.length } })
        }
        view.focus(); break
      }
      case 'strikethrough': wrap('~~', '~~'); break
      case 'inline_code': wrap('`', '`'); break
      case 'blockquote': prefixLine('> '); break
      case 'ul': prefixLine('- '); break
      case 'ol': prefixLine('1. '); break
      case 'link': {
        if (sel) {
          view.dispatch({ changes: { from, to, insert: `[${sel}]()` }, selection: { anchor: from + sel.length + 3 } })
        } else {
          view.dispatch({ changes: { from, to, insert: '[](url)' }, selection: { anchor: from + 1 } })
        }
        view.focus(); break
      }
      case 'codeblock': {
        const insert = sel ? `\`\`\`\n${sel}\n\`\`\`` : '```\n\n```'
        view.dispatch({ changes: { from, to, insert }, selection: { anchor: from + 4 + (sel ? sel.length : 0) } })
        view.focus(); break
      }
      case 'table': {
        const table = '| 欄位 | 欄位 |\n|------|------|\n| 內容 | 內容 |'
        view.dispatch({ changes: { from, to, insert: table }, selection: { anchor: from + table.length } })
        view.focus(); break
      }
      case 'hr': {
        const line = view.state.doc.lineAt(from)
        view.dispatch({ changes: { from: line.to, to: line.to, insert: '\n\n---\n' }, selection: { anchor: line.to + 6 } })
        view.focus(); break
      }
    }
  }, [handleAction])

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

      <div style={{ flex: 1, overflow: 'hidden', position: 'relative', display: 'flex', flexDirection: 'column' }}>
        {/* CM6 Editor (editor + live modes) */}
        <div
          ref={editorRef}
          onContextMenu={(e) => { if (!currentPath) return; e.preventDefault(); setCmCtxMenu({ x: e.clientX, y: e.clientY }) }}
          style={{
            display: (viewMode === 'editor' || viewMode === 'live') ? 'block' : 'none',
            width: '100%', height: '100%',
          }}
        />

        {/* 浮動模式切換按鈕（Source 模式下提示切回編輯） */}
        {viewMode === 'editor' && currentPath && (
          <div style={{ position: 'absolute', top: '10px', right: '16px', zIndex: 10, display: 'flex', gap: '6px' }}>
            <button
              onClick={() => setViewMode('live')}
              title="切換為編輯模式"
              style={{
                padding: '4px 10px', borderRadius: '6px', fontSize: '12px',
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                color: 'var(--color-text-secondary)', cursor: 'pointer', opacity: 0.75,
                transition: 'opacity 0.15s',
              }}
              onMouseEnter={e => (e.currentTarget.style.opacity = '1')}
              onMouseLeave={e => (e.currentTarget.style.opacity = '0.75')}
            >✎ 編輯</button>
            <button
              onClick={() => setViewMode('preview')}
              title="切換為預覽模式"
              style={{
                padding: '4px 10px', borderRadius: '6px', fontSize: '12px',
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                color: 'var(--color-text-secondary)', cursor: 'pointer', opacity: 0.75,
                transition: 'opacity 0.15s',
              }}
              onMouseEnter={e => (e.currentTarget.style.opacity = '1')}
              onMouseLeave={e => (e.currentTarget.style.opacity = '0.75')}
            >👁 預覽</button>
          </div>
        )}

        {/* Preview Panel */}
        {viewMode === 'preview' && currentPath && (
          <PreviewPanel
            content={content}
            onWikilinkClick={wikilinkHandler}
            onEdit={() => setViewMode('live')}
            pendingAnchor={pendingAnchor}
            onAnchorScrolled={() => setPendingAnchor(undefined)}
            onTextStyle={handleTextStyle}
            onEditQuickCopy={handleEditQuickCopy}
          />
        )}
      </div>

      <BacklinksPanel onOpenNote={openNote} />

      {/* CM6 右鍵選單 */}
      {cmCtxMenu && (
        <div
          ref={cmMenuRef}
          onMouseDown={e => e.stopPropagation()}
          style={{
            position: 'fixed', zIndex: 99999,
            left: Math.min(cmCtxMenu.x, window.innerWidth - 220),
            top: Math.min(cmCtxMenu.y, window.innerHeight - 460),
            background: 'var(--color-bg-elevated)',
            border: '1px solid var(--color-border)',
            borderRadius: '8px',
            boxShadow: '0 6px 24px rgba(0,0,0,0.28)',
            minWidth: 200, overflow: 'visible', padding: '4px 0',
          }}
        >
              {/* 格式 */}
          {([
            { label: '粗體', action: 'bold', shortcut: '⌘B' },
            { label: '斜體', action: 'italic', shortcut: '⌘I' },
            { label: '刪除線', action: 'strikethrough' },
            { label: '行內程式碼', action: 'inline_code' },
          ] as Array<{ label: string; action: string; shortcut?: string }>).map(item => (
            <div key={item.action}
              onClick={() => applyCmAction(item.action)}
              style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
              onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-bg-hover)'; setCmSubMenuOpen(false) }}
              onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
            >
              <span>{item.label}</span>
              {item.shortcut && <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', marginLeft: '16px' }}>{item.shortcut}</span>}
            </div>
          ))}

          <div style={{ height: '1px', background: 'var(--color-border)', margin: '3px 0' }} />

          {/* 標題（submenu） */}
          <div style={{ position: 'relative' }}
            onMouseEnter={openSubMenu}
            onMouseLeave={closeSubMenu}
          >
            <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)', background: cmSubMenuOpen ? 'var(--color-bg-hover)' : 'transparent' }}>
              <span>標題</span>
              <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>›</span>
            </div>
            {cmSubMenuOpen && (
              <div
                onMouseEnter={openSubMenu}
                onMouseLeave={closeSubMenu}
                style={{
                  position: 'absolute', left: '100%', top: 0,
                  background: 'var(--color-bg-elevated)',
                  border: '1px solid var(--color-border)',
                  borderRadius: '8px',
                  boxShadow: '0 6px 24px rgba(0,0,0,0.28)',
                  minWidth: 120, overflow: 'hidden', padding: '4px 0', zIndex: 99999,
                }}>
                {(['h1','h2','h3','h4','h5','h6'] as const).map((h, i) => (
                  <div key={h}
                    onClick={() => applyCmAction(h)}
                    style={{ padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
                    onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                    onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                  >
                    H{i + 1}
                  </div>
                ))}
              </div>
            )}
          </div>

          <div style={{ height: '1px', background: 'var(--color-border)', margin: '3px 0' }} />

          {/* 區塊 */}
          {([
            { label: '引言', action: 'blockquote' },
            { label: '無序清單', action: 'ul' },
            { label: '有序清單', action: 'ol' },
          ] as Array<{ label: string; action: string }>).map(item => (
            <div key={item.action}
              onClick={() => applyCmAction(item.action)}
              style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
              onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-bg-hover)'; setCmSubMenuOpen(false) }}
              onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
            >
              <span>{item.label}</span>
            </div>
          ))}

          <div style={{ height: '1px', background: 'var(--color-border)', margin: '3px 0' }} />

          {/* 插入 */}
          {([
            { label: '插入連結', action: 'link' },
            { label: '插入 Wikilink', action: 'wikilink' },
            { label: '插入圖片', action: 'image' },
            { label: '插入快捷複製', action: 'quick_copy' },
            { label: '插入程式碼區塊', action: 'codeblock' },
            { label: '插入表格', action: 'table' },
          ] as Array<{ label: string; action: string }>).map(item => (
            <div key={item.action}
              onClick={() => applyCmAction(item.action)}
              style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
              onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-bg-hover)'; setCmSubMenuOpen(false) }}
              onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
            >
              <span>{item.label}</span>
            </div>
          ))}

          <div style={{ height: '1px', background: 'var(--color-border)', margin: '3px 0' }} />

          <div
            onClick={() => applyCmAction('hr')}
            style={{ display: 'flex', alignItems: 'center', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
            onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-bg-hover)'; setCmSubMenuOpen(false) }}
            onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
          >
            插入分隔線
          </div>
        </div>
      )}

      {/* Image Modal */}
      {imageModal && (
        <div style={{
          position: 'fixed', inset: 0, zIndex: 9999,
          background: 'rgba(0,0,0,0.45)', display: 'flex',
          alignItems: 'center', justifyContent: 'center',
        }} onClick={() => setImageModal(null)}>
          <div style={{
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
            borderRadius: '10px', padding: '20px 24px', width: '380px',
            display: 'flex', flexDirection: 'column', gap: '12px',
          }} onClick={(e) => e.stopPropagation()}>
            <div style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>插入圖片</div>

            {/* 來源選擇 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              來源
              <select value={imgSource} onChange={e => { setImgSource(e.target.value as 'file' | 'url'); setImgUrlError('') }}
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}>
                <option value="file">系統圖片</option>
                <option value="url">使用網址</option>
              </select>
            </label>

            {/* 系統圖片 picker */}
            {imgSource === 'file' && (
              <div style={{ display: 'flex', flexDirection: 'column', gap: '6px' }}>
                <div style={{ display: 'flex', gap: '8px', alignItems: 'center' }}>
                  <button onClick={pickImageFile}
                    style={{ padding: '5px 12px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px', cursor: 'pointer', flexShrink: 0 }}>
                    選擇圖片…
                  </button>
                  <span style={{ fontSize: '12px', color: 'var(--color-text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>
                    {imgFilePath || '尚未選擇'}
                  </span>
                </div>
              </div>
            )}

            {/* 網址輸入 */}
            {imgSource === 'url' && (
              <div style={{ display: 'flex', flexDirection: 'column', gap: '4px' }}>
                <input value={imgUrl} onChange={e => { setImgUrl(e.target.value); setImgUrlError('') }}
                  placeholder="https://example.com/image.png"
                  style={{ padding: '5px 8px', borderRadius: '5px', border: `1px solid ${imgUrlError ? 'var(--color-error, #e04040)' : 'var(--color-border)'}`, background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
                {imgUrlError && <span style={{ fontSize: '11px', color: 'var(--color-error, #e04040)' }}>{imgUrlError}</span>}
              </div>
            )}

            {/* 圖片大小 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              大小（px 或 %）
              <input value={imgSize} onChange={e => setImgSize(e.target.value)}
                placeholder="例：300 或 50%"
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
            </label>

            {/* 顯示名稱 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示名稱
              <input value={imgAlt} onChange={e => setImgAlt(e.target.value)}
                placeholder="（選填）"
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
            </label>

            {/* 按鈕 */}
            <div style={{ display: 'flex', gap: '8px', justifyContent: 'flex-end', marginTop: '4px' }}>
              <button onClick={() => setImageModal(null)}
                style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)', cursor: 'pointer' }}>
                取消
              </button>
              <button
                onClick={() => {
                  if (imgSource === 'url' && imgUrl.trim() && !/^https?:\/\/.+/.test(imgUrl.trim())) {
                    setImgUrlError('請輸入有效的 http/https 網址')
                    return
                  }
                  confirmImageInsert()
                }}
                style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: '#fff', background: 'var(--color-accent)', border: 'none', cursor: 'pointer' }}>
                確認
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Quick Copy Modal */}
      {quickCopyModal && (
        <div style={{
          position: 'fixed', inset: 0, zIndex: 9999,
          background: 'rgba(0,0,0,0.45)', display: 'flex',
          alignItems: 'center', justifyContent: 'center',
        }} onClick={() => setQuickCopyModal(null)}>
          <div style={{
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
            borderRadius: '10px', padding: '20px 24px', width: '360px',
            display: 'flex', flexDirection: 'column', gap: '12px',
          }} onClick={(e) => e.stopPropagation()}>
            <div style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>插入快捷複製</div>

            {/* 顯示文字 */}
            <label style={{ display: 'flex', flexDirection: 'column', gap: '4px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字
              <input value={qcDisplayText} onChange={(e) => setQcDisplayText(e.target.value)}
                placeholder="（空白則使用可複製的內文）"
                style={{ padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
            </label>

            {/* 顯示文字顏色 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字顏色
              <input type="color" value={qcColor} onChange={(e) => setQcColor(e.target.value)}
                style={{ width: '36px', height: '28px', padding: '1px', border: '1px solid var(--color-border)', borderRadius: '4px', cursor: 'pointer', background: 'none' }} />
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: '12px', color: 'var(--color-text-muted)' }}>{qcColor}</span>
            </label>

            {/* 顯示文字大小 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字大小（px）
              <input type="number" value={qcFontSize} onChange={(e) => setQcFontSize(e.target.value)}
                placeholder="預設"
                style={{ width: '80px', padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
            </label>

            {/* 顯示文字字型 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字字型
              <select value={qcFontFamily} onChange={(e) => setQcFontFamily(e.target.value)}
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}>
                <option value="inherit">預設</option>
                <option value="serif">Serif</option>
                <option value="sans-serif">Sans-serif</option>
                <option value="monospace">Monospace</option>
              </select>
            </label>

            {/* 顯示文字粗細 */}
            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字粗細
              <select value={qcFontWeight} onChange={(e) => setQcFontWeight(e.target.value)}
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}>
                <option value="inherit">預設</option>
                <option value="normal">Normal</option>
                <option value="bold">Bold</option>
                <option value="600">Semi-bold (600)</option>
              </select>
            </label>

            {/* 可複製的內文 */}
            <label style={{ display: 'flex', flexDirection: 'column', gap: '4px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              可複製的內文
              <textarea value={qcCopyContent} onChange={(e) => setQcCopyContent(e.target.value)}
                rows={3}
                style={{ padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px', resize: 'vertical', fontFamily: 'var(--font-mono)' }} />
            </label>

            {/* 預覽 */}
            <div style={{ fontSize: '12px', color: 'var(--color-text-muted)', display: 'flex', alignItems: 'center', gap: '8px' }}>
              預覽：
              <span className="quick-copy" style={{
                color: qcColor || undefined,
                fontSize: qcFontSize ? `${qcFontSize}px` : undefined,
                fontFamily: qcFontFamily !== 'inherit' ? qcFontFamily : undefined,
                fontWeight: qcFontWeight !== 'inherit' ? qcFontWeight : undefined,
              }}>
                {qcDisplayText || qcCopyContent || '範例文字'}
              </span>
            </div>

            {/* 按鈕 */}
            <div style={{ display: 'flex', gap: '8px', justifyContent: 'flex-end', marginTop: '4px' }}>
              <button onClick={() => setQuickCopyModal(null)}
                style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)' }}>
                取消
              </button>
              <button onClick={confirmQuickCopy}
                style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: '#fff', background: 'var(--color-accent)', border: 'none', cursor: 'pointer' }}>
                確認
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
