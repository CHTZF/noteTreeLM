import { useEffect, useRef, useCallback, useState } from 'react'
import { EditorView, basicSetup } from 'codemirror'
import { EditorState, Compartment } from '@codemirror/state'
import { markdown, markdownLanguage } from '@codemirror/lang-markdown'
import { languages } from '@codemirror/language-data'
import { keymap, EditorView as EditorViewCm } from '@codemirror/view'
import { defaultKeymap, history, historyKeymap } from '@codemirror/commands'
import { open as openDialog } from '@tauri-apps/plugin-dialog'
import { open as openPath } from '@tauri-apps/plugin-shell'
import { invoke } from '@tauri-apps/api/core'
import { useEditorStore } from '../../stores/editorStore'
import { useVaultStore, getCachedNoteContent } from '../../stores/vaultStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { NOTE_TEMPLATES } from '../../utils/noteTemplates'
import { wikilinkPlugin } from './plugins/wikilinks'
import { livePreviewPlugin, livePreviewTheme, setLiveEditQuickCopyHandler, setLiveEditImageHandler } from './plugins/livePreview'
import type { EditorAction } from './Toolbar'
import PreviewPanel from './PreviewPanel'
import BacklinksPanel from '../BacklinksPanel/BacklinksPanel'
import { toast } from '../common/Toast'

// Editor styles are defined in App.css (.cm-editor selectors) to avoid
// Windows WebView2 Constructable Stylesheet failures (EditorView.theme uses adoptedStyleSheets).

const MIME_MAP: Record<string, string> = {
  png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg',
  gif: 'image/gif', webp: 'image/webp', svg: 'image/svg+xml', bmp: 'image/bmp',
}

function VaultThumb({ relPath }: { relPath: string }) {
  const [src, setSrc] = useState('')
  useEffect(() => {
    invoke<string>('read_vault_file_base64', { relPath })
      .then(b64 => {
        const ext = relPath.split('.').pop()?.toLowerCase() ?? 'png'
        setSrc(`data:${MIME_MAP[ext] ?? 'image/png'};base64,${b64}`)
      })
      .catch(() => {})
  }, [relPath])
  return src
    ? <img src={src} alt="" style={{ width: '28px', height: '28px', objectFit: 'cover', borderRadius: '3px', flexShrink: 0 }} />
    : <div style={{ width: '28px', height: '28px', flexShrink: 0 }} />
}

interface EditorProps {
  canGoBack?: boolean
  canGoForward?: boolean
  onBack?: () => void
  onForward?: () => void
  onOpenNote: (path: string, opts?: { source?: import('../../stores/activityStore').ActivitySource; fromPath?: string }) => void
}

export default function Editor({ onOpenNote }: EditorProps) {
  const editorRef = useRef<HTMLDivElement>(null)
  const viewRef = useRef<EditorView | null>(null)
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Ref so the CM6 updateListener always calls the latest version (avoids stale closure)
  const triggerAutoSaveRef = useRef<(content: string) => void>(() => {})
  const handleActionRef = useRef<(action: EditorAction) => void>(() => {})
  // Guard: true while we are programmatically loading content into CM6.
  // Prevents updateListener from treating the load dispatch as a user edit and
  // scheduling an auto-save that would overwrite the previous note with empty content.
  const isLoadingRef = useRef(false)
  // Compartment to toggle live preview extension dynamically
  const liveCompartment = useRef(new Compartment())

  const { currentPath, content, isDirty, viewMode, setContent, setDirty, setViewMode,
          pendingContent, clearPendingContent, pendingAnchor, setPendingAnchor, applyExternalWrite } = useEditorStore()
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

  // ── Image resize modal（右鍵已有圖片時）─────────────────────────────────────
  const [imgResizeModal, setImgResizeModal] = useState<{ from: number; to: number; altBase: string } | null>(null)
  const [imgResizeSize, setImgResizeSize] = useState('')

  // ── Image insert modal（工具列插入新圖片時）──────────────────────────────────
  const [imageModal, setImageModal] = useState<{ from: number; to: number } | null>(null)
  const [imgSize, setImgSize] = useState('')
  const [vaultImages, setVaultImages] = useState<string[]>([])
  const [vaultImgFilter, setVaultImgFilter] = useState('')
  const [vaultImgSelected, setVaultImgSelected] = useState('')
  // import sub-page state
  const [importPage, setImportPage] = useState(false)
  const [importMode, setImportMode] = useState<'file' | 'url' | null>(null)
  const [importUrl, setImportUrl] = useState('')
  const [importUrlError, setImportUrlError] = useState('')
  const [importBusy, setImportBusy] = useState(false)
  const [importError, setImportError] = useState('')
  const [importName, setImportName] = useState('')
  const [importFilePath, setImportFilePath] = useState('')

  const openNote = useCallback((path: string) => {
    onOpenNote(path)
    setPendingAnchor(undefined)  // 清除殘留 anchor（非錨點導航時）
  }, [onOpenNote])

  // 載入筆記（帶 cancellation 防止快速切換時 race condition）
  useEffect(() => {
    // Cancel any pending auto-save from the previous note before switching
    if (saveTimerRef.current) {
      clearTimeout(saveTimerRef.current)
      saveTimerRef.current = null
    }

    const loadContent = (text: string) => {
      isLoadingRef.current = true
      try {
        if (viewRef.current) {
          viewRef.current.dispatch({
            changes: { from: 0, to: viewRef.current.state.doc.length, insert: text }
          })
        }
      } finally {
        isLoadingRef.current = false
      }
    }

    if (!currentPath) {
      loadContent('')
      return
    }

    // Cache hit: fill synchronously — no flash of previous content
    const cached = getCachedNoteContent(currentPath)
    if (cached !== undefined) {
      loadContent(cached)
      setContent(cached)
      setDirty(false)
      return
    }

    // Cache miss: clear immediately so stale content from the previous note doesn't linger
    loadContent('')

    let cancelled = false
    readNote(currentPath).then((note) => {
      if (cancelled) return
      loadContent(note.content)
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

    const IMAGE_EXTS = new Set(['png','jpg','jpeg','gif','webp','svg','bmp','ico','tiff','avif'])
    const isImageMime = (mime: string) => mime.startsWith('image/')
    const isImageFilename = (name: string) => {
      const ext = name.split('.').pop()?.toLowerCase() ?? ''
      return IMAGE_EXTS.has(ext)
    }

    const handleFilePaste = async (files: FileList, view: EditorView) => {
      for (const file of Array.from(files)) {
        try {
          // 讀成 base64
          const arrayBuf = await file.arrayBuffer()
          const bytes = new Uint8Array(arrayBuf)
          let binary = ''
          bytes.forEach(b => { binary += String.fromCharCode(b) })
          const b64 = btoa(binary)

          const isImage = isImageMime(file.type) || isImageFilename(file.name)
          const folder = isImage ? 'assets' : ''

          const relPath = await invoke<string>('import_file_from_bytes', {
            filename: file.name,
            folder,
            dataBase64: b64,
          })

          // 插入 wikilink 或圖片語法到游標
          const insert = isImage
            ? `![[${relPath}]]`
            : `[[${relPath}]]`

          const { from } = view.state.selection.main
          view.dispatch({
            changes: { from, to: from, insert: insert + ' ' },
            selection: { anchor: from + insert.length + 1 },
          })
        } catch (err) {
          toast.error(`匯入失敗：${file.name}`)
          console.error(err)
        }
      }
    }

    const view = new EditorView({
      state: EditorState.create({
        doc: '',
        extensions: [
          basicSetup,
          history(),
          markdown({ base: markdownLanguage, codeLanguages: languages }),
          livePreviewTheme,
          wikilinkPlugin,
          liveCompartment.current.of([]), // initially no live preview
          EditorViewCm.lineWrapping,
          keymap.of([...defaultKeymap, ...historyKeymap]),
          EditorView.updateListener.of((update) => {
            if (update.docChanged && !isLoadingRef.current) {
              const newContent = update.state.doc.toString()
              setContent(newContent)
              triggerAutoSaveRef.current(newContent)
            }
          }),
          EditorViewCm.domEventHandlers({
            paste(event, view) {
              const files = event.clipboardData?.files
              if (!files || files.length === 0) return false
              // 只攔截含有檔案的 paste（純文字讓 CM6 自己處理）
              event.preventDefault()
              handleFilePaste(files, view)
              return true
            },
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
      // When becoming visible (switching from preview→live/editor), the editor
      // may have been hidden with display:none, causing stale measurements.
      // On Windows WebView2 the layout may need two frames to settle, so we
      // use a double-rAF: first frame triggers measurement, second frame
      // dispatches a no-op selection update to force liveInlinePlugin to
      // rebuild decorations with the now-correct visibleRanges.
      if (viewMode === 'live' || viewMode === 'editor') {
        requestAnimationFrame(() => {
          view.requestMeasure()
          requestAnimationFrame(() => {
            if (viewRef.current === view) {
              // Setting selection to its current value sets selectionSet=true
              // in the ViewUpdate, which triggers liveInlinePlugin to rebuild.
              view.dispatch({ selection: view.state.selection })
            }
          })
        })
      }
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

  // 全域快捷鍵（依設定）
  useEffect(() => {
    const matchHotkey = (e: KeyboardEvent, combo: string) => {
      if (!combo) return false
      const parts = combo.split('+')
      const key = parts[parts.length - 1]
      const needMod   = parts.includes('mod')
      const needShift = parts.includes('shift')
      const needAlt   = parts.includes('alt')
      const needCtrl  = parts.includes('ctrl')
      if ((e.metaKey || e.ctrlKey) !== needMod) return false
      if (e.shiftKey !== needShift) return false
      if (e.altKey   !== needAlt)   return false
      if (needCtrl && !e.ctrlKey)   return false
      return e.key.toLowerCase() === key
    }
    const handler = (e: KeyboardEvent) => {
      if (matchHotkey(e, settings.hotkey_save ?? 'mod+s')) {
        e.preventDefault(); save(); return
      }
      if (matchHotkey(e, settings.hotkey_toggle_view ?? 'mod+e')) {
        e.preventDefault()
        setViewMode(viewMode === 'live' ? 'preview' : 'live')
        return
      }
      if (matchHotkey(e, settings.hotkey_bold ?? 'mod+b')) {
        e.preventDefault(); handleActionRef.current('bold'); return
      }
      if (matchHotkey(e, settings.hotkey_italic ?? 'mod+i')) {
        e.preventDefault(); handleActionRef.current('italic'); return
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [save, settings.hotkey_save, settings.hotkey_toggle_view, settings.hotkey_bold, settings.hotkey_italic, viewMode, setViewMode])

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
        setImgSize('')
        setVaultImgFilter('')
        setVaultImgSelected('')
        setImportPage(false)
        setImportMode(null)
        setImportUrl('')
        setImportUrlError('')
        setImportError('')
        setImportName('')
        setImportFilePath('')
        setImageModal({ from, to })
        loadVaultImages()
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
  const [cmCtxMenu, setCmCtxMenu] = useState<{ x: number; y: number; from: number; to: number; sel: string; mode: 'menu' | 'color' | 'font' } | null>(null)
  const cmMenuRef = useRef<HTMLDivElement>(null)
  const [cmSubMenuOpen, setCmSubMenuOpen] = useState(false)
  const subMenuCloseTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const openSubMenu = () => { if (subMenuCloseTimer.current) clearTimeout(subMenuCloseTimer.current); setCmSubMenuOpen(true) }
  const closeSubMenu = () => { subMenuCloseTimer.current = setTimeout(() => setCmSubMenuOpen(false), 120) }
  // Color/font picker state
  const [cmPickColor, setCmPickColor] = useState('#e03030')
  const [cmFontFamily, setCmFontFamily] = useState('inherit')
  const [cmFontSize, setCmFontSize] = useState('')
  const [cmFontWeight, setCmFontWeight] = useState('inherit')

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

  // Register live-mode right-click edit handler for quick-copy widgets
  useEffect(() => {
    setLiveEditQuickCopyHandler((data) => {
      // Parse existing style back into individual fields
      const get = (prop: string) => {
        const m = data.style.match(new RegExp(prop + ':([^;]+)'))
        return m ? m[1].trim() : ''
      }
      setQcCopyContent(data.dataCopy)
      setQcDisplayText(data.displayText)
      setQcColor(get('color') || '#2080e0')
      setQcFontSize(get('font-size').replace('px', ''))
      setQcFontFamily(get('font-family') || 'inherit')
      setQcFontWeight(get('font-weight') || 'inherit')
      setQuickCopyModal({ from: data.from, to: data.to })
    })
    return () => setLiveEditQuickCopyHandler(null)
  }, [])

  const IMAGE_EXTENSIONS = ['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'bmp', 'avif']

  const loadVaultImages = useCallback(async () => {
    try {
      const all = await invoke<string[]>('list_assets')
      setVaultImages(all.filter(p => IMAGE_EXTENSIONS.includes(p.split('.').pop()?.toLowerCase() ?? '')))
    } catch {}
  }, [])

  // Register live-mode right-click edit handler for image widgets
  // 右鍵已有圖片 → 只開輕量 resize modal
  useEffect(() => {
    setLiveEditImageHandler((data) => {
      const barIdx = data.alt.lastIndexOf('|')
      const hasSize = barIdx !== -1 && /^\d/.test(data.alt.slice(barIdx + 1))
      const altBase = hasSize ? data.alt.slice(0, barIdx) : data.alt
      const size    = hasSize ? data.alt.slice(barIdx + 1) : ''
      setImgResizeSize(size)
      setImgResizeModal({ from: data.from, to: data.to, altBase })
    })
    return () => setLiveEditImageHandler(null)
  }, [])

  const handlePickFile = useCallback(async () => {
    try {
      const file = await openDialog({
        multiple: false,
        filters: [{ name: 'Images', extensions: IMAGE_EXTENSIONS }],
      })
      if (!file) return
      const srcPath = typeof file === 'string' ? file : (file as any).path ?? String(file)
      const basename = srcPath.split(/[\\/]/).pop() ?? ''
      setImportFilePath(srcPath)
      setImportName(basename)
      setImportMode('file')
      setImportError('')
    } catch (e: any) {
      setImportError(e?.message ?? String(e))
    }
  }, [])

  const handleConfirmFileCopy = useCallback(async () => {
    if (!importFilePath) return
    setImportBusy(true)
    setImportError('')
    try {
      const relPath = await invoke<string>('import_image', {
        sourcePath: importFilePath,
        folder: 'assets',
        newName: importName.trim() || null,
      })
      await loadVaultImages()
      setVaultImgSelected(relPath)
      setImportPage(false)
      setImportMode(null)
      setImportFilePath('')
      setImportName('')
    } catch (e: any) {
      setImportError(e?.message ?? String(e))
    } finally {
      setImportBusy(false)
    }
  }, [importFilePath, importName, loadVaultImages])

  const handleImportUrl = useCallback(async () => {
    const url = importUrl.trim()
    if (!url) return
    if (!/^https?:\/\/.+/.test(url)) {
      setImportUrlError('請輸入有效的 http/https 網址')
      return
    }
    setImportBusy(true)
    setImportError('')
    setImportUrlError('')
    try {
      const relPath = await invoke<string>('download_asset_to_vault', {
        url,
        newName: importName.trim() || null,
      })
      await loadVaultImages()
      setVaultImgSelected(relPath)
      setImportPage(false)
      setImportMode(null)
      setImportUrl('')
      setImportName('')
    } catch (e: any) {
      setImportError(e?.message ?? String(e))
    } finally {
      setImportBusy(false)
    }
  }, [importUrl, importName, loadVaultImages])

  const confirmImageInsert = useCallback(() => {
    if (!imageModal || !vaultImgSelected) return
    const view = viewRef.current
    if (!view) return
    const size = imgSize.trim()
    const suffix = size ? `|${size}` : ''
    const insert = `![[${vaultImgSelected}${suffix}]]`
    view.dispatch({
      changes: { from: imageModal.from, to: imageModal.to, insert },
      selection: { anchor: imageModal.from + insert.length },
    })
    setImageModal(null)
    view.focus()
  }, [imageModal, vaultImgSelected, imgSize])

  const confirmImgResize = useCallback(() => {
    if (!imgResizeModal) return
    const view = viewRef.current
    if (!view) return
    const size = imgResizeSize.trim()
    const suffix = size ? `|${size}` : ''
    // 重建 wikilink：保留原本的 relPath，只改 size
    // 原來的 markdown 是 ![[relPath]] 或 ![[relPath|oldSize]]
    // 從 doc 中讀出原始文字，取得 relPath
    const orig = view.state.doc.sliceString(imgResizeModal.from, imgResizeModal.to)
    const m = orig.match(/^!\[\[(.+?)(?:\|\d+)?\]\]$/)
    const relPath = m ? m[1] : imgResizeModal.altBase
    const insert = `![[${relPath}${suffix}]]`
    view.dispatch({
      changes: { from: imgResizeModal.from, to: imgResizeModal.to, insert },
      selection: { anchor: imgResizeModal.from + insert.length },
    })
    setImgResizeModal(null)
    view.focus()
  }, [imgResizeModal, imgResizeSize])

  // 每次 handleAction 更新時同步給 ref（供 keydown handler 使用）
  useEffect(() => { handleActionRef.current = handleAction }, [handleAction])

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

  const wikilinkHandler = async (title: string, anchor?: string) => {
    // If the title has a file extension, treat it as a non-note file and open with system default app
    if (/\.[^/.]+$/.test(title)) {
      try {
        const assets = await invoke<string[]>('list_assets')
        const lower = title.toLowerCase()
        const match = assets.find(a => a.toLowerCase() === lower || a.toLowerCase().endsWith('/' + lower))
        if (match) {
          const vaultPath = useSettingsStore.getState().settings.system_current_vault_path.replace(/\\/g, '/')
          await openPath(`${vaultPath}/${match}`)
        }
      } catch { /* ignore */ }
      return
    }
    const note = useVaultStore.getState().notes.find((n) => n.title === title)
    if (!note) return
    if (note.path === currentPath) {
      setPendingAnchor(anchor)
      return
    }
    onOpenNote(note.path, { source: 'wikilink', fromPath: currentPath ?? undefined })
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
          onContextMenu={(e) => {
            if (!currentPath) return
            e.preventDefault()
            const view = viewRef.current
            if (!view) return
            const { from, to } = view.state.selection.main
            const sel = view.state.doc.sliceString(from, to)
            setCmCtxMenu({ x: e.clientX, y: e.clientY, from, to, sel, mode: 'menu' })
            setCmPickColor('#e03030')
            setCmFontFamily('inherit'); setCmFontSize(''); setCmFontWeight('inherit')
          }}
          style={{
            display: (viewMode === 'editor' || viewMode === 'live') ? 'block' : 'none',
            width: '100%', height: '100%',
          }}
        />

        {/* 浮動模式切換按鈕（live 模式右上角顯示「預覽」，Source 模式顯示「編輯」+「預覽」） */}
        {viewMode === 'live' && currentPath && (
          <div style={{ position: 'absolute', top: '10px', right: '16px', zIndex: 10, display: 'flex', gap: '6px', alignItems: 'center' }}>
            <select
              defaultValue=""
              onChange={e => {
                const id = e.target.value
                if (!id) return
                e.target.value = ''
                const tmpl = NOTE_TEMPLATES.find(t => t.id === id)
                if (!tmpl) return
                const title = currentPath.split('/').pop()?.replace(/\.(md|markdown|mdx)$/i, '') ?? '未命名'
                if (content.trim() && !window.confirm(`套用「${tmpl.label}」模板會取代現有內容，確定嗎？`)) return
                applyExternalWrite(tmpl.content(title))
              }}
              title="套用筆記模板"
              style={{
                padding: '4px 8px', borderRadius: '6px', fontSize: '12px',
                background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
                color: 'var(--color-text-secondary)', cursor: 'pointer', opacity: 0.75,
              }}
            >
              <option value="" disabled>套用模板…</option>
              {NOTE_TEMPLATES.map(t => (
                <option key={t.id} value={t.id}>{t.label}</option>
              ))}
            </select>
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
            left: Math.min(cmCtxMenu.x, window.innerWidth - 240),
            top: Math.min(cmCtxMenu.y, window.innerHeight - 520),
            background: 'var(--color-bg-elevated)',
            border: '1px solid var(--color-border)',
            borderRadius: '8px',
            boxShadow: '0 6px 24px rgba(0,0,0,0.28)',
            minWidth: 220, overflow: 'visible', padding: '4px 0',
          }}
        >

          {/* ── 主選單 ── */}
          {cmCtxMenu.mode === 'menu' && (<>
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

            {/* 改變顏色 / 改變字型（僅當有選取文字時） */}
            {cmCtxMenu.sel && (<>
              <div
                onClick={() => setCmCtxMenu(m => m && ({ ...m, mode: 'color' }))}
                style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
                onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-bg-hover)'; setCmSubMenuOpen(false) }}
                onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
              >
                <span>改變顏色</span>
                <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>›</span>
              </div>
              <div
                onClick={() => setCmCtxMenu(m => m && ({ ...m, mode: 'font' }))}
                style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '6px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
                onMouseEnter={e => { e.currentTarget.style.background = 'var(--color-bg-hover)'; setCmSubMenuOpen(false) }}
                onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
              >
                <span>改變字型</span>
                <span style={{ fontSize: '11px', color: 'var(--color-text-muted)' }}>›</span>
              </div>
              <div style={{ height: '1px', background: 'var(--color-border)', margin: '3px 0' }} />
            </>)}

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
          </>)}

          {/* ── 改變顏色 panel ── */}
          {cmCtxMenu.mode === 'color' && (
            <div style={{ padding: '12px', display: 'flex', flexDirection: 'column', gap: 10 }}>
              <div style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)' }}>改變顏色</div>
              <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                {['#e03030','#e07830','#d4c020','#30a850','#2080e0','#8040e0','#e030a0','#888888','#000000'].map(c => (
                  <div key={c} onClick={() => setCmPickColor(c)}
                    style={{ width: 22, height: 22, borderRadius: 4, background: c, cursor: 'pointer',
                      border: cmPickColor === c ? '2px solid var(--color-text-primary)' : '2px solid transparent' }} />
                ))}
              </div>
              <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                自訂
                <input type="color" value={cmPickColor} onChange={e => setCmPickColor(e.target.value)}
                  style={{ width: 32, height: 24, padding: 0, border: '1px solid var(--color-border)', borderRadius: 4, cursor: 'pointer' }} />
                <span style={{ fontFamily: 'monospace' }}>{cmPickColor}</span>
              </label>
              <div style={{ padding: '6px 10px', borderRadius: 6, background: 'var(--color-bg-base)', fontSize: '13px', border: '1px solid var(--color-border)' }}>
                <span style={{ color: cmPickColor }}>{cmCtxMenu.sel.slice(0, 30)}{cmCtxMenu.sel.length > 30 ? '…' : ''}</span>
              </div>
              <div style={{ display: 'flex', gap: 6 }}>
                <button type="button" onClick={() => {
                  const view = viewRef.current; if (!view) return
                  const { from, to, sel } = cmCtxMenu
                  view.dispatch({ changes: { from, to, insert: `{color:${cmPickColor}}${sel}{/color}` } })
                  setCmCtxMenu(null); view.focus()
                }} style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-accent)', color: 'white', fontSize: '12px', cursor: 'pointer' }}>
                  套用
                </button>
                <button type="button" onClick={() => setCmCtxMenu(m => m && ({ ...m, mode: 'menu' }))}
                  style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-bg-hover)', color: 'var(--color-text-secondary)', fontSize: '12px', cursor: 'pointer' }}>
                  返回
                </button>
              </div>
            </div>
          )}

          {/* ── 改變字型 panel ── */}
          {cmCtxMenu.mode === 'font' && (
            <div style={{ padding: '12px', display: 'flex', flexDirection: 'column', gap: 10 }}>
              <div style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)' }}>改變字型</div>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                字型
                <select value={cmFontFamily} onChange={e => setCmFontFamily(e.target.value)}
                  style={{ padding: '4px 6px', borderRadius: 5, border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '12px' }}>
                  <option value="inherit">預設</option>
                  <option value="serif">Serif</option>
                  <option value="sans-serif">Sans-serif</option>
                  <option value="monospace">Monospace</option>
                  <option value="Noto Serif TC">Noto Serif TC</option>
                  <option value="Source Han Sans TC">Source Han Sans</option>
                </select>
              </label>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                大小 (px)
                <input type="number" min={8} max={72} placeholder="例: 16" value={cmFontSize}
                  onChange={e => setCmFontSize(e.target.value)}
                  style={{ padding: '4px 6px', borderRadius: 5, border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '12px', width: '100%', boxSizing: 'border-box' }} />
              </label>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                粗細
                <select value={cmFontWeight} onChange={e => setCmFontWeight(e.target.value)}
                  style={{ padding: '4px 6px', borderRadius: 5, border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '12px' }}>
                  <option value="inherit">預設</option>
                  <option value="300">細 (300)</option>
                  <option value="normal">正常 (400)</option>
                  <option value="500">中等 (500)</option>
                  <option value="bold">粗 (700)</option>
                  <option value="900">極粗 (900)</option>
                </select>
              </label>
              <div style={{ display: 'flex', gap: 6 }}>
                <button type="button" onClick={() => {
                  const view = viewRef.current; if (!view) return
                  const { from, to, sel } = cmCtxMenu
                  const spec = [cmFontFamily, cmFontSize, cmFontWeight].join(';')
                  view.dispatch({ changes: { from, to, insert: `{font:${spec}}${sel}{/font}` } })
                  setCmCtxMenu(null); view.focus()
                }} style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-accent)', color: 'white', fontSize: '12px', cursor: 'pointer' }}>
                  套用
                </button>
                <button type="button" onClick={() => setCmCtxMenu(m => m && ({ ...m, mode: 'menu' }))}
                  style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-bg-hover)', color: 'var(--color-text-secondary)', fontSize: '12px', cursor: 'pointer' }}>
                  返回
                </button>
              </div>
            </div>
          )}

        </div>
      )}

      {/* Image Resize Modal（右鍵已有圖片） */}
      {imgResizeModal && (
        <div style={{ position:'fixed',inset:0,background:'rgba(0,0,0,0.5)',zIndex:99998,display:'flex',alignItems:'center',justifyContent:'center' }}
          onMouseDown={() => setImgResizeModal(null)}>
          <div onMouseDown={e => e.stopPropagation()}
            style={{ background:'var(--color-bg-elevated)',border:'1px solid var(--color-border)',borderRadius:10,padding:'20px 24px',minWidth:260,display:'flex',flexDirection:'column',gap:14 }}>
            <div style={{ fontSize:14,fontWeight:600,color:'var(--color-text-primary)' }}>調整圖片大小</div>
            <label style={{ display:'flex',flexDirection:'column',gap:6,fontSize:12,color:'var(--color-text-muted)' }}>
              寬度（px，留空為原始大小）
              <input
                type="number" min={1} max={9999}
                value={imgResizeSize}
                onChange={e => setImgResizeSize(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') confirmImgResize(); if (e.key === 'Escape') setImgResizeModal(null) }}
                autoFocus
                placeholder="例如 300"
                style={{ padding:'6px 10px',borderRadius:6,border:'1px solid var(--color-border)',background:'var(--color-bg-base)',color:'var(--color-text-primary)',fontSize:13 }}
              />
            </label>
            <div style={{ display:'flex',gap:8,justifyContent:'flex-end' }}>
              <button onClick={() => setImgResizeModal(null)}
                style={{ padding:'6px 14px',borderRadius:6,border:'1px solid var(--color-border)',background:'none',color:'var(--color-text-secondary)',fontSize:13,cursor:'pointer' }}>
                取消
              </button>
              <button onClick={confirmImgResize}
                style={{ padding:'6px 14px',borderRadius:6,border:'none',background:'var(--color-accent)',color:'white',fontSize:13,cursor:'pointer' }}>
                套用
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Image Modal */}
      {imageModal && (() => {
        const filtered = vaultImages.filter(p => p.toLowerCase().includes(vaultImgFilter.toLowerCase()))
        const btnBase: React.CSSProperties = {
          display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center',
          gap: '8px', padding: '20px 16px', borderRadius: '10px', cursor: 'pointer',
          border: '1px solid var(--color-border)', background: 'var(--color-bg-base)',
          color: 'var(--color-text-primary)', fontSize: '13px', flex: 1,
          transition: 'border-color 0.15s, background 0.15s',
        }
        return (
          <div style={{
            position: 'fixed', inset: 0, zIndex: 9999,
            background: 'rgba(0,0,0,0.45)', display: 'flex',
            alignItems: 'center', justifyContent: 'center',
          }} onClick={() => setImageModal(null)}>
            <div style={{
              background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
              borderRadius: '10px', width: '420px', overflow: 'hidden',
            }} onClick={e => e.stopPropagation()}>

              {/* Slide container */}
              <div style={{
                display: 'flex',
                transform: importPage ? 'translateX(-420px)' : 'translateX(0)',
                transition: 'transform 0.22s ease',
                width: '840px',
              }}>

                {/* ── Panel A: 工作區圖片（主畫面）──────────────────────── */}
                <div style={{ width: '420px', flexShrink: 0, padding: '20px 20px 16px', display: 'flex', flexDirection: 'column', gap: '10px' }}>
                  <div style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>插入圖片</div>

                  {/* 搜尋框 */}
                  <input
                    value={vaultImgFilter}
                    onChange={e => setVaultImgFilter(e.target.value)}
                    placeholder="搜尋工作區圖片…"
                    style={{ padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}
                  />

                  {/* 圖片列表 */}
                  <div style={{ height: '180px', overflowY: 'auto', border: '1px solid var(--color-border)', borderRadius: '6px', background: 'var(--color-bg-base)' }}>
                    {filtered.length === 0 ? (
                      <div style={{ height: '100%', display: 'flex', alignItems: 'center', justifyContent: 'center', fontSize: '12px', color: 'var(--color-text-muted)' }}>
                        {vaultImages.length === 0 ? '工作區內沒有圖片，請先匯入' : '無符合結果'}
                      </div>
                    ) : filtered.map(p => (
                      <div key={p} onClick={() => setVaultImgSelected(p)}
                        style={{
                          display: 'flex', alignItems: 'center', gap: '8px',
                          padding: '6px 10px', cursor: 'pointer', fontSize: '12px',
                          background: vaultImgSelected === p ? 'var(--color-accent)' : 'transparent',
                          color: vaultImgSelected === p ? '#fff' : 'var(--color-text-primary)',
                        }}
                        onMouseEnter={e => { if (vaultImgSelected !== p) e.currentTarget.style.background = 'var(--color-bg-hover)' }}
                        onMouseLeave={e => { if (vaultImgSelected !== p) e.currentTarget.style.background = 'transparent' }}
                      >
                        <VaultThumb relPath={p} />
                        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>{p.split('/').pop()}</span>
                        <span style={{ opacity: 0.45, flexShrink: 0, fontSize: '11px' }}>{p.includes('/') ? p.substring(0, p.lastIndexOf('/')) : ''}</span>
                      </div>
                    ))}
                  </div>

                  {/* 大小 */}
                  <label style={{ display: 'flex', alignItems: 'center', gap: '6px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
                    大小
                    <input value={imgSize} onChange={e => setImgSize(e.target.value)} placeholder="300 或 50%" style={{ flex: 1, padding: '4px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
                  </label>

                  {/* 預覽格式 */}
                  {vaultImgSelected && (
                    <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', lineHeight: 1.4 }}>
                      插入：<code style={{ color: 'var(--color-accent)' }}>
                        {`![[${vaultImgSelected}${imgSize.trim() ? `|${imgSize.trim()}` : ''}]]`}
                      </code>
                    </div>
                  )}

                  {/* 底部按鈕 */}
                  <div style={{ display: 'flex', alignItems: 'center', marginTop: '2px' }}>
                    <button onClick={() => { setImportPage(true); setImportMode(null); setImportError('') }}
                      style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)', cursor: 'pointer' }}>
                      匯入
                    </button>
                    <div style={{ flex: 1 }} />
                    <button onClick={() => setImageModal(null)}
                      style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)', cursor: 'pointer', marginRight: '8px' }}>
                      取消
                    </button>
                    <button onClick={confirmImageInsert} disabled={!vaultImgSelected}
                      style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: '#fff', background: 'var(--color-accent)', border: 'none', cursor: vaultImgSelected ? 'pointer' : 'not-allowed', opacity: vaultImgSelected ? 1 : 0.5 }}>
                      確認
                    </button>
                  </div>
                </div>

                {/* ── Panel B: 匯入（子畫面）───────────────────────────── */}
                <div style={{ width: '420px', flexShrink: 0, padding: '20px 20px 16px', display: 'flex', flexDirection: 'column', gap: '14px' }}>
                  {/* 標題列 */}
                  <div style={{ display: 'flex', alignItems: 'center', gap: '8px' }}>
                    <button onClick={() => { setImportPage(false); setImportMode(null); setImportError('') }}
                      style={{ background: 'none', border: 'none', cursor: 'pointer', color: 'var(--color-text-muted)', fontSize: '18px', lineHeight: 1, padding: '0 4px 0 0' }}>
                      ←
                    </button>
                    <span style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>匯入圖片</span>
                  </div>

                  {/* 兩個大 icon 按鈕 */}
                  {importMode === null && (
                    <div style={{ display: 'flex', gap: '12px' }}>
                      <button
                        onClick={handlePickFile}
                        style={btnBase}
                        onMouseEnter={e => { e.currentTarget.style.borderColor = 'var(--color-accent)'; e.currentTarget.style.background = 'var(--color-accent-dim, rgba(10,132,255,0.08))' }}
                        onMouseLeave={e => { e.currentTarget.style.borderColor = 'var(--color-border)'; e.currentTarget.style.background = 'var(--color-bg-base)' }}
                      >
                        <span style={{ fontSize: '32px' }}>📁</span>
                        <span style={{ fontWeight: 500 }}>系統圖片</span>
                        <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', textAlign: 'center' }}>從本機選取並複製到工作區</span>
                      </button>
                      <button
                        onClick={() => { setImportMode('url'); setImportName(''); setImportUrl(''); setImportUrlError('') }}
                        style={btnBase}
                        onMouseEnter={e => { e.currentTarget.style.borderColor = 'var(--color-accent)'; e.currentTarget.style.background = 'var(--color-accent-dim, rgba(10,132,255,0.08))' }}
                        onMouseLeave={e => { e.currentTarget.style.borderColor = 'var(--color-border)'; e.currentTarget.style.background = 'var(--color-bg-base)' }}
                      >
                        <span style={{ fontSize: '32px' }}>🔗</span>
                        <span style={{ fontWeight: 500 }}>網址圖片</span>
                        <span style={{ fontSize: '11px', color: 'var(--color-text-muted)', textAlign: 'center' }}>下載網路圖片到工作區</span>
                      </button>
                    </div>
                  )}

                  {/* 系統圖片：選好檔案後顯示表單 */}
                  {importMode === 'file' && (
                    <div style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
                      <div style={{ fontSize: '12px', color: 'var(--color-text-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={importFilePath}>
                        {importFilePath.split(/[\\/]/).pop()}
                      </div>
                      <label style={{ display: 'flex', alignItems: 'center', gap: '6px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
                        名稱
                        <input
                          value={importName}
                          onChange={e => setImportName(e.target.value)}
                          placeholder="（選填）"
                          autoFocus
                          style={{ flex: 1, padding: '4px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}
                          onKeyDown={e => { if (e.key === 'Enter') handleConfirmFileCopy() }}
                        />
                      </label>
                      <div style={{ display: 'flex', gap: '8px', justifyContent: 'flex-end' }}>
                        <button onClick={() => { setImportMode(null); setImportFilePath(''); setImportName('') }}
                          style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)', cursor: 'pointer' }}>
                          返回
                        </button>
                        <button onClick={handleConfirmFileCopy} disabled={importBusy}
                          style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: '#fff', background: 'var(--color-accent)', border: 'none', cursor: importBusy ? 'not-allowed' : 'pointer', opacity: importBusy ? 0.5 : 1 }}>
                          {importBusy ? '複製中…' : '複製'}
                        </button>
                      </div>
                    </div>
                  )}

                  {/* URL 輸入（選擇網址模式後顯示） */}
                  {importMode === 'url' && (
                    <div style={{ display: 'flex', flexDirection: 'column', gap: '8px' }}>
                      <input
                        value={importUrl}
                        onChange={e => { setImportUrl(e.target.value); setImportUrlError('') }}
                        placeholder="https://example.com/image.png"
                        autoFocus
                        style={{ padding: '7px 10px', borderRadius: '6px', border: `1px solid ${importUrlError ? 'var(--color-error, #e04040)' : 'var(--color-border)'}`, background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}
                      />
                      {importUrlError && <span style={{ fontSize: '11px', color: 'var(--color-error, #e04040)' }}>{importUrlError}</span>}
                      <label style={{ display: 'flex', alignItems: 'center', gap: '6px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
                        名稱
                        <input
                          value={importName}
                          onChange={e => setImportName(e.target.value)}
                          placeholder="（選填）"
                          style={{ flex: 1, padding: '4px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}
                          onKeyDown={e => { if (e.key === 'Enter') handleImportUrl() }}
                        />
                      </label>
                      <div style={{ display: 'flex', gap: '8px', justifyContent: 'flex-end' }}>
                        <button onClick={() => setImportMode(null)}
                          style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)', cursor: 'pointer' }}>
                          返回
                        </button>
                        <button onClick={handleImportUrl} disabled={importBusy || !importUrl.trim()}
                          style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: '#fff', background: 'var(--color-accent)', border: 'none', cursor: (!importBusy && importUrl.trim()) ? 'pointer' : 'not-allowed', opacity: (!importBusy && importUrl.trim()) ? 1 : 0.5 }}>
                          {importBusy ? '下載中…' : '下載'}
                        </button>
                      </div>
                    </div>
                  )}
                  {importError && (
                    <div style={{ fontSize: '12px', color: 'var(--color-error, #e04040)', lineHeight: 1.5 }}>{importError}</div>
                  )}
                </div>

              </div>
            </div>
          </div>
        )
      })()}

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
