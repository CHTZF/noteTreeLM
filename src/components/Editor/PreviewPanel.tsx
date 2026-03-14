import { useEffect, useRef, useState, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import MarkdownIt from 'markdown-it'
import mk from 'markdown-it-texmath'
import katex from 'katex'
import mermaid from 'mermaid'
import DOMPurify from 'dompurify'
import { open } from '@tauri-apps/plugin-shell'
import { useSettingsStore } from '../../stores/settingsStore'

mermaid.initialize({ startOnLoad: false, theme: 'neutral' })

const md = new MarkdownIt({
  html: true,
  linkify: true,
  typographer: true,
  breaks: true,
}).use(mk, { engine: katex, delimiters: 'dollars', katexOptions: { throwOnError: false } })

interface PreviewPanelProps {
  content: string
  onWikilinkClick: (title: string, anchor?: string) => void
  onEdit?: () => void
  pendingAnchor?: string
  onAnchorScrolled?: () => void
  /** Called when user applies inline style via right-click menu */
  onTextStyle?: (text: string, styleStr: string, contextBefore: string) => void
  /** Called when user edits a quick-copy span via right-click: (dataCopyDecoded, newHtml) */
  onEditQuickCopy?: (dataCopyDecoded: string, newHtml: string) => void
}

// ─── Edit Quick-Copy modal state ─────────────────────────────────────────────
interface EditQcState {
  dataCopy: string   // original decoded data-copy value
  displayText: string
  color: string
  fontSize: string
  fontFamily: string
  fontWeight: string
  copyContent: string
}

// ─── Text-style context-menu state ───────────────────────────────────────────
interface CtxMenu {
  x: number; y: number; text: string; contextBefore: string
  mode: 'menu' | 'font' | 'color'
}

function slugifyHeading(text: string): string {
  return text
    .replace(/<[^>]*>/g, '')
    .toLowerCase().trim()
    .replace(/\s+/g, '-')
    .replace(/[^\w\u4e00-\u9fff-]/g, '')
    .replace(/-+/g, '-').replace(/^-|-$/g, '')
}

function guessMimeType(path: string): string {
  const ext = path.split('.').pop()?.toLowerCase() || ''
  const map: Record<string, string> = {
    png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg',
    gif: 'image/gif', webp: 'image/webp', svg: 'image/svg+xml', bmp: 'image/bmp',
  }
  return map[ext] || 'image/png'
}

// 預處理：對標準 markdown 圖片路徑中的空格做 URL 編碼
// markdown-it 遇到未編碼的空格會停止解析 URL，導致整個 ![alt](url) 變成純文字
const preprocessImageUrls = (content: string): string => {
  return content.replace(/!\[([^\]]*)\]\(([^)]+)\)/g, (_, alt, url) => {
    if (url.startsWith('http') || url.startsWith('data:')) return `![${alt}](${url})`
    // 只編碼空格（及 tab），保留中文等字元，讓 markdown-it 能正確解析
    const encoded = url.replace(/[ \t]/g, '%20')
    return `![${alt}](${encoded})`
  })
}

// post-render 處理 wikilinks，避免 html:false 轉義注入的 HTML
const processWikilinks = (renderedHtml: string) => {
  // ![[image.png|500]] → <img src="image.png" data-img-size="500">
  let result = renderedHtml.replace(/!\[\[([^\[\]|]+?)(?:\|(\d+(?:x\d+)?))?\]\]/g, (_, filename, size) => {
    const sizeAttr = size ? ` data-img-size="${size}"` : ''
    return `<img src="${filename.trim()}" alt="${filename.trim()}"${sizeAttr} />`
  })
  // [[title#anchor|alias]] / [[title#anchor]] / [[title|alias]] / [[title]] → <a>
  // 同時支援 ASCII # (U+0023) 與全形 ＃ (U+FF03) 作為錨點分隔符
  result = result.replace(/\[\[([^\[\]|#＃]+)(?:[#＃]([^\[\]|]*))?\|?([^\[\]]*)\]\]/g, (_, title, anchor, alias) => {
    const display = (alias || title).trim()
    const anchorAttr = anchor ? ` data-wikilink-anchor="${anchor.trim()}"` : ''
    return `<a href="#" data-wikilink="${title.trim()}"${anchorAttr} class="wikilink">${display}</a>`
  })
  return result
}

// 標題 id + block id：在掛載到 DOM 前先於 detached div 處理，
// 避免 useEffect 在首次 paint 後才跑導致 ^id 文字閃現
function processHtmlForPreview(html: string): string {
  const tmp = document.createElement('div')
  tmp.innerHTML = html

  // 為每個標題加上唯一 id（同名標題加 -1, -2 ... 後綴）
  const usedIds = new Map<string, number>()
  tmp.querySelectorAll('h1,h2,h3,h4,h5,h6').forEach((h) => {
    if (!h.id) {
      const base = slugifyHeading(h.textContent || '') || 'heading'
      const count = usedIds.get(base) ?? 0
      usedIds.set(base, count + 1)
      h.id = count === 0 ? base : `${base}-${count}`
    }
  })

  // 處理段落 block id
  Array.from(tmp.querySelectorAll('p')).forEach((p) => {
    const text = p.textContent ?? ''

    // 行內 block id：段落最後的文字節點以 "\s^block-id" 結尾
    // 用 \s 而非空格，支援 ^id 寫在下一行（無空行）時 markdown-it 插入 \n 的情況
    const lastChild = p.lastChild
    if (lastChild?.nodeType === Node.TEXT_NODE) {
      const m = lastChild.textContent?.match(/\s\^([\w-]+)$/)
      if (m) {
        p.id = m[1]
        lastChild.textContent = lastChild.textContent!.replace(/\s\^[\w-]+$/, '')
        return
      }
    }
    // 獨立 block id 行（^id）：將 id 移至前一個元素（表格、段落等）並移除此段落
    if (/^\^[\w-]+$/.test(text.trim())) {
      const blockId = text.trim().slice(1)
      const prev = p.previousElementSibling
      if (prev && !prev.id) prev.id = blockId
      p.remove()
    }
  })

  // 處理表格欄位 block id（^id 可緊接在文字後，空格可選）
  Array.from(tmp.querySelectorAll('td, th')).forEach((cell) => {
    const lastChild = cell.lastChild
    if (lastChild?.nodeType === Node.TEXT_NODE) {
      const m = lastChild.textContent?.match(/\s?\^([\w-]+)$/)
      if (m) {
        cell.id = m[1]
        lastChild.textContent = lastChild.textContent!.replace(/\s?\^[\w-]+$/, '')
      }
    }
  })

  return tmp.innerHTML
}

// 將圖片 src 解析為可用的圖片 URL
// - 相對路徑 → read_vault_file_base64（Rust PathBuf::join，跨平台可靠）
// - 絕對路徑 → read_file_base64
// - vault://localhost/ → 擷取相對路徑後走相對路徑邏輯
// - src 可能被 markdown-it 的 encodeURI 編碼，先用 decodeURI 還原
async function resolveImageSrc(src: string, _vaultPath: string): Promise<string> {
  if (src.startsWith('data:') || src.startsWith('asset:')) return src
  // http:// 僅允許外部 URL（tauri.localhost 表示相對路徑解析失敗，仍需重試）
  if (src.startsWith('http') && !src.startsWith('http://tauri.localhost') && !src.startsWith('https://tauri.localhost')) return src

  // markdown-it 會對 image URL 呼叫 encodeURI，此處還原為原始路徑
  let decoded = src
  try { decoded = decodeURI(src) } catch { /* 保持原值 */ }

  // vault:// scheme（macOS WKWebView 用）→ 轉為相對路徑
  if (decoded.startsWith('vault://localhost/')) {
    decoded = decodeURIComponent(decoded.slice('vault://localhost/'.length))
  }

  const isAbsolute = decoded.startsWith('/') || /^[A-Za-z]:[/\\]/.test(decoded)

  if (isAbsolute) {
    // 絕對路徑：直接呼叫 read_file_base64
    const normalizedPath = decoded.replace(/\\/g, '/')
    try {
      const base64 = await invoke<string>('read_file_base64', { path: normalizedPath })
      return `data:${guessMimeType(normalizedPath)};base64,${base64}`
    } catch (e) {
      console.warn('無法讀取圖片:', normalizedPath, e)
      return src
    }
  }

  // 相對路徑：使用 read_vault_file_base64（Rust 側組合路徑，不依賴前端字串拼接）
  try {
    const base64 = await invoke<string>('read_vault_file_base64', { relPath: decoded })
    return `data:${guessMimeType(decoded)};base64,${base64}`
  } catch (e) {
    // Obsidian bare filename fallback：搜尋整個 vault
    const isBareFilename = !decoded.includes('/') && !decoded.includes('\\')
    if (isBareFilename) {
      try {
        const assets = await invoke<string[]>('list_assets')
        const lower = decoded.toLowerCase()
        const match = assets.find(a => a.toLowerCase() === lower || a.toLowerCase().endsWith('/' + lower))
        if (match) {
          const base64 = await invoke<string>('read_vault_file_base64', { relPath: match })
          return `data:${guessMimeType(match)};base64,${base64}`
        }
      } catch { /* fallthrough */ }
    }
    console.warn('無法讀取圖片:', decoded, e)
    return src
  }
}

export default function PreviewPanel({ content, onWikilinkClick, onEdit, pendingAnchor, onAnchorScrolled, onTextStyle, onEditQuickCopy }: PreviewPanelProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const { settings } = useSettingsStore()

  // ─── Right-click style menu ───────────────────────────────────────────────
  const [ctxMenu, setCtxMenu] = useState<CtxMenu | null>(null)
  const [fontFamily, setFontFamily] = useState('inherit')
  const [fontSize, setFontSize] = useState('')
  const [fontWeight, setFontWeight] = useState('inherit')
  const [textColor, setTextColor] = useState('#e03030')
  const menuRef = useRef<HTMLDivElement>(null)

  // ─── Edit Quick-Copy modal ────────────────────────────────────────────────
  const [editQcModal, setEditQcModal] = useState<EditQcState | null>(null)

  // ─── TOC + Collapsible headings ───────────────────────────────────────────
  const [tocItems, setTocItems] = useState<{ id: string; level: number; text: string }[]>([])
  const [tocOpen, setTocOpen] = useState(false)
  const [collapsedHeadings, setCollapsedHeadings] = useState<Set<string>>(new Set())

  // Close on outside click
  useEffect(() => {
    if (!ctxMenu) return
    const close = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setCtxMenu(null)
    }
    setTimeout(() => window.addEventListener('mousedown', close), 0)
    return () => window.removeEventListener('mousedown', close)
  }, [ctxMenu])

  const confirmEditQc = useCallback(() => {
    if (!editQcModal || !onEditQuickCopy) return
    const parts: string[] = []
    if (editQcModal.color) parts.push(`color:${editQcModal.color}`)
    if (editQcModal.fontSize) parts.push(`font-size:${editQcModal.fontSize}px`)
    if (editQcModal.fontFamily && editQcModal.fontFamily !== 'inherit') parts.push(`font-family:${editQcModal.fontFamily}`)
    if (editQcModal.fontWeight && editQcModal.fontWeight !== 'inherit') parts.push(`font-weight:${editQcModal.fontWeight}`)
    const styleAttr = parts.length ? ` style="${parts.join(';')}"` : ''
    const escapedCopy = editQcModal.copyContent.replace(/&/g, '&amp;').replace(/"/g, '&quot;')
    const displayText = editQcModal.displayText || editQcModal.copyContent
    const newHtml = `<span class="quick-copy" data-copy="${escapedCopy}"${styleAttr}>${displayText}</span>`
    onEditQuickCopy(editQcModal.dataCopy, newHtml)
    setEditQcModal(null)
  }, [editQcModal, onEditQuickCopy])

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    // Quick-copy right-click: open edit modal
    const copyEl = (e.target as HTMLElement).closest('[data-copy]') as HTMLElement | null
    if (copyEl && onEditQuickCopy) {
      e.preventDefault()
      const raw = copyEl.getAttribute('data-copy') ?? ''
      setEditQcModal({
        dataCopy: raw,
        displayText: copyEl.textContent ?? '',
        color: copyEl.style.color || '#2080e0',
        fontSize: copyEl.style.fontSize ? copyEl.style.fontSize.replace('px', '') : '',
        fontFamily: copyEl.style.fontFamily.replace(/['"]/g, '') || 'inherit',
        fontWeight: copyEl.style.fontWeight || 'inherit',
        copyContent: raw,
      })
      return
    }

    if (!onTextStyle) return
    const selection = window.getSelection()
    const sel = selection?.toString()?.trim() ?? ''
    if (!sel || !selection || selection.rangeCount === 0) return
    e.preventDefault()
    // Capture plain text before the selection within the container (for position disambiguation)
    let contextBefore = ''
    try {
      const range = selection.getRangeAt(0)
      if (containerRef.current) {
        const beforeRange = document.createRange()
        beforeRange.setStart(containerRef.current, 0)
        beforeRange.setEnd(range.startContainer, range.startOffset)
        contextBefore = beforeRange.toString()
      }
    } catch { /* ignore */ }
    setCtxMenu({ x: e.clientX, y: e.clientY, text: sel, contextBefore, mode: 'menu' })
    setFontFamily('inherit'); setFontSize(''); setFontWeight('inherit'); setTextColor('#e03030')
  }, [onTextStyle, onEditQuickCopy])

  const applyFont = useCallback(() => {
    if (!ctxMenu) return
    const parts: string[] = []
    if (fontFamily && fontFamily !== 'inherit') parts.push(`font-family:${fontFamily}`)
    if (fontSize) parts.push(`font-size:${fontSize}px`)
    if (fontWeight && fontWeight !== 'inherit') parts.push(`font-weight:${fontWeight}`)
    if (parts.length > 0) onTextStyle?.(ctxMenu.text, parts.join(';'), ctxMenu.contextBefore)
    setCtxMenu(null)
  }, [ctxMenu, fontFamily, fontSize, fontWeight, onTextStyle])

  const applyColor = useCallback(() => {
    if (!ctxMenu) return
    onTextStyle?.(ctxMenu.text, `color:${textColor}`, ctxMenu.contextBefore)
    setCtxMenu(null)
  }, [ctxMenu, textColor, onTextStyle])

  // 渲染管線：
  //   preprocessImageUrls → md.render → processWikilinks → DOMPurify → processHtmlForPreview
  // processHtmlForPreview 在 detached div 上同步處理標題 id 和 block id，
  // 確保 ^id 文字在首次 paint 前就被移除（避免 useEffect 延遲造成的閃現）
  const html = processHtmlForPreview(
    DOMPurify.sanitize(
      processWikilinks(md.render(preprocessImageUrls(content))),
      { ADD_ATTR: ['data-wikilink', 'data-wikilink-anchor', 'class', 'data-img-size', 'style', 'data-copy'] }
    )
  )

  // 圖片後處理：套用尺寸 + 解析圖片路徑為 data URL
  useEffect(() => {
    if (!containerRef.current) return
    const vaultPath = settings.system_current_vault_path

    const processImages = async () => {
      const imgs = Array.from(containerRef.current!.querySelectorAll('img'))
      for (const img of imgs) {
        // 套用尺寸：來自 data-img-size（wikilink embed）或 alt text |size
        const embedSize = img.getAttribute('data-img-size')
        const altText = img.getAttribute('alt') || ''
        const altMatch = altText.match(/^(.*?)\|(\d+)(?:x(\d+))?$/)

        if (embedSize) {
          const [w, h] = embedSize.split('x')
          img.style.width = w + 'px'
          if (h) img.style.height = h + 'px'
          img.removeAttribute('data-img-size')
        } else if (altMatch) {
          const [, cleanAlt, w, h] = altMatch
          img.setAttribute('alt', cleanAlt.trim())
          img.style.width = w + 'px'
          if (h) img.style.height = h + 'px'
        }

        // 解析圖片路徑
        const src = img.getAttribute('src') || ''
        const resolved = await resolveImageSrc(src, vaultPath)
        if (resolved !== src) {
          img.setAttribute('src', resolved)
        }
      }
    }

    processImages()
  }, [html, settings.system_current_vault_path])

  useEffect(() => {
    if (!containerRef.current) return

    // 渲染 Mermaid 圖表
    const mermaidBlocks = containerRef.current.querySelectorAll('code.language-mermaid')
    mermaidBlocks.forEach(async (block, i) => {
      const pre = block.parentElement
      if (!pre) return
      const code = block.textContent || ''
      try {
        const { svg } = await mermaid.render(`mermaid-${Date.now()}-${i}`, code)
        const div = document.createElement('div')
        div.className = 'mermaid-diagram'
        div.innerHTML = svg
        pre.replaceWith(div)
      } catch {
        // 保留原始程式碼
      }
    })

    // 處理連結點擊 & 快捷複製
    const handleClick = (e: MouseEvent) => {
      const el = e.target as HTMLElement

      // Quick-copy: 點擊 [data-copy] 元素 → 複製內文到剪貼簿
      const copyEl = el.closest('[data-copy]') as HTMLElement | null
      if (copyEl) {
        const copyText = copyEl.getAttribute('data-copy') ?? ''
        navigator.clipboard.writeText(copyText).catch(() => {})
        copyEl.style.opacity = '0.4'
        setTimeout(() => { copyEl.style.opacity = '' }, 150)
        return
      }

      const target = el.closest('a')
      if (!target) return
      e.preventDefault()

      const wikilink = target.getAttribute('data-wikilink')
      if (wikilink) {
        const anchor = target.getAttribute('data-wikilink-anchor')
        onWikilinkClick(wikilink, anchor || undefined)
        return
      }

      const href = target.getAttribute('href')
      if (href && (href.startsWith('http://') || href.startsWith('https://'))) {
        open(href)
      }
    }

    const container = containerRef.current
    container.addEventListener('click', handleClick)
    return () => container.removeEventListener('click', handleClick)
  }, [html, onWikilinkClick])

  // 捲動到錨點（[[Note#Heading]] 或 [[Note#^block-id]]）
  useEffect(() => {
    if (!pendingAnchor || !containerRef.current) return
    const id = pendingAnchor.startsWith('^')
      ? pendingAnchor.slice(1)
      : slugifyHeading(pendingAnchor)
    const timer = setTimeout(() => {
      const el = containerRef.current?.querySelector(`#${CSS.escape(id)}`)
      if (el) {
        el.scrollIntoView({ behavior: 'smooth', block: 'start' })
        onAnchorScrolled?.()
      }
    }, 50)
    return () => clearTimeout(timer)
  }, [html, pendingAnchor])

  // TOC 擷取 + 標題點擊收合
  useEffect(() => {
    if (!containerRef.current) return
    const container = containerRef.current

    // 擷取 TOC（先清空，避免前一篇殘留）
    setTocItems([])
    setCollapsedHeadings(new Set())
    const items: { id: string; level: number; text: string }[] = []
    container.querySelectorAll('h1,h2,h3,h4,h5,h6').forEach((h) => {
      const el = h as HTMLElement
      if (!el.id) return
      items.push({ id: el.id, level: parseInt(el.tagName[1]), text: el.textContent?.trim() ?? '' })
      el.style.cursor = 'pointer'
    })
    setTocItems(items)

    // 事件委派：標題點擊 → 收合
    const handleHeadingClick = (e: MouseEvent) => {
      const target = e.target as HTMLElement
      if (target.closest('[data-copy]') || target.closest('a')) return
      const heading = target.closest('h1,h2,h3,h4,h5,h6') as HTMLElement | null
      if (!heading?.id) return
      setCollapsedHeadings(prev => {
        const next = new Set(prev)
        if (next.has(heading.id)) next.delete(heading.id)
        else next.add(heading.id)
        return next
      })
    }
    container.addEventListener('click', handleHeadingClick)
    return () => container.removeEventListener('click', handleHeadingClick)
  }, [html])

  // 套用收合狀態到 DOM
  useEffect(() => {
    if (!containerRef.current) return
    const container = containerRef.current
    const children = Array.from(container.children) as HTMLElement[]

    // 先全部重設
    children.forEach(el => { el.style.display = '' })

    // 依序套用收合（由上到下處理）
    children.forEach((el, idx) => {
      if (!/^H[1-6]$/.test(el.tagName) || !el.id) return
      const isCollapsed = collapsedHeadings.has(el.id)
      el.setAttribute('data-collapsed', String(isCollapsed))
      if (!isCollapsed) return
      const level = parseInt(el.tagName[1])
      for (let i = idx + 1; i < children.length; i++) {
        const child = children[i]
        if (/^H[1-6]$/.test(child.tagName) && parseInt(child.tagName[1]) <= level) break
        child.style.display = 'none'
      }
    })
  }, [html, collapsedHeadings])

  const scrollToHeading = useCallback((id: string) => {
    const el = containerRef.current?.querySelector(`#${CSS.escape(id)}`)
    if (el) el.scrollIntoView({ behavior: 'smooth', block: 'start' })
  }, [])

  return (
    <div style={{ flex: 1, position: 'relative', display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
      {/* 標題收合 CSS */}
      <style>{`
        .preview-content h1,.preview-content h2,.preview-content h3,
        .preview-content h4,.preview-content h5,.preview-content h6 { cursor:pointer; }
        .preview-content h1::before,.preview-content h2::before,.preview-content h3::before,
        .preview-content h4::before,.preview-content h5::before,.preview-content h6::before
          { content:'▼ '; font-size:0.6em; opacity:0.35; display:inline-block; width:14px; vertical-align:middle; }
        .preview-content h1[data-collapsed="true"]::before,.preview-content h2[data-collapsed="true"]::before,
        .preview-content h3[data-collapsed="true"]::before,.preview-content h4[data-collapsed="true"]::before,
        .preview-content h5[data-collapsed="true"]::before,.preview-content h6[data-collapsed="true"]::before
          { content:'▶ '; }
      `}</style>

      {onEdit && (
        <button
          onClick={onEdit}
          title="開啟編輯器"
          style={{
            position: 'absolute', top: '10px', right: tocOpen ? '206px' : '46px', zIndex: 10,
            display: 'flex', alignItems: 'center', gap: '5px',
            padding: '5px 12px', borderRadius: '6px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
            color: 'var(--color-text-secondary)', fontSize: '12px',
            cursor: 'pointer', transition: 'right 0.2s, all 0.15s',
            boxShadow: 'var(--shadow-sm)',
          }}
          onMouseEnter={(e) => {
            (e.currentTarget as HTMLElement).style.background = 'var(--color-bg-overlay)'
            ;(e.currentTarget as HTMLElement).style.color = 'var(--color-text-primary)'
          }}
          onMouseLeave={(e) => {
            (e.currentTarget as HTMLElement).style.background = 'var(--color-bg-elevated)'
            ;(e.currentTarget as HTMLElement).style.color = 'var(--color-text-secondary)'
          }}
        >
          <span style={{ fontSize: '13px' }}>✎</span>
          <span>編輯</span>
        </button>
      )}

      {/* 主體：預覽 + TOC 側欄 */}
      <div style={{ flex: 1, display: 'flex', overflow: 'hidden' }}>
        <div
          ref={containerRef}
          className="preview-content"
          dangerouslySetInnerHTML={{ __html: html }}
          onContextMenu={handleContextMenu}
          style={{
            flex: 1,
            overflowY: 'auto',
            padding: '24px 32px',
            fontFamily: "'Inter', sans-serif",
            fontSize: '14px',
            lineHeight: 1.7,
            color: 'var(--color-text-primary)',
          }}
        />

        {/* TOC 側欄 */}
        <div style={{
          width: tocOpen ? '190px' : '28px',
          flexShrink: 0,
          borderLeft: '1px solid var(--color-border)',
          background: 'var(--color-bg-base)',
          display: 'flex',
          flexDirection: 'column',
          overflow: 'hidden',
          transition: 'width 0.2s ease',
        }}>
          {/* 側欄標頭 */}
          <div style={{
            display: 'flex', alignItems: 'center',
            padding: tocOpen ? '8px 10px' : '8px 4px',
            borderBottom: '1px solid var(--color-border)',
            flexShrink: 0, gap: '6px',
          }}>
            <button
              onClick={() => setTocOpen(o => !o)}
              title={tocOpen ? '收合大綱' : '展開大綱'}
              style={{
                flexShrink: 0, width: '20px', height: '20px',
                display: 'flex', alignItems: 'center', justifyContent: 'center',
                borderRadius: '4px', fontSize: '12px',
                color: 'var(--color-text-secondary)',
                background: 'transparent',
              }}
            >{tocOpen ? '›' : '☰'}</button>
            {tocOpen && <span style={{ fontSize: '11px', fontWeight: 600, color: 'var(--color-text-muted)', letterSpacing: '0.04em', whiteSpace: 'nowrap' }}>大綱</span>}
          </div>

          {/* TOC 條目 */}
          {tocOpen && (
            <div style={{ flex: 1, overflowY: 'auto', padding: '6px 4px' }}>
              {tocItems.length === 0 ? (
                <div style={{ fontSize: '11px', color: 'var(--color-text-muted)', padding: '8px 8px', fontStyle: 'italic' }}>無標題</div>
              ) : tocItems.map((item, idx) => (
                <div
                  key={idx}
                  onClick={() => scrollToHeading(item.id)}
                  title={item.text}
                  style={{
                    paddingLeft: `${(item.level - 1) * 10 + 8}px`,
                    paddingRight: '6px',
                    paddingTop: '3px',
                    paddingBottom: '3px',
                    fontSize: '11.5px',
                    lineHeight: 1.4,
                    color: 'var(--color-text-secondary)',
                    cursor: 'pointer',
                    borderRadius: '4px',
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                    transition: 'background 0.1s',
                  }}
                  onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                  onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                >
                  {item.text}
                </div>
              ))}
            </div>
          )}
        </div>
      </div>

      {/* ── Right-click style menu ── */}
      {ctxMenu && (
        <div
          ref={menuRef}
          style={{
            position: 'fixed', zIndex: 99999,
            left: Math.min(ctxMenu.x, window.innerWidth - 240),
            top: Math.min(ctxMenu.y, window.innerHeight - 320),
            background: 'var(--color-bg-elevated)',
            border: '1px solid var(--color-border)',
            borderRadius: '8px',
            boxShadow: '0 6px 24px rgba(0,0,0,0.3)',
            minWidth: 220,
            overflow: 'hidden',
          }}
          onMouseDown={e => e.stopPropagation()}
        >
          {ctxMenu.mode === 'menu' && (
            <>
              <div style={{ padding: '8px 12px', fontSize: '11px', color: 'var(--color-text-muted)', borderBottom: '1px solid var(--color-border)', userSelect: 'none' }}>
                已選取：「{ctxMenu.text.slice(0, 20)}{ctxMenu.text.length > 20 ? '…' : ''}」
              </div>
              {[
                { label: '複製文字', action: () => { navigator.clipboard.writeText(ctxMenu.text); setCtxMenu(null) } },
                { label: '改變字型 (字型/大小/粗細)', action: () => setCtxMenu(m => m && ({ ...m, mode: 'font' })) },
                { label: '改變顏色', action: () => setCtxMenu(m => m && ({ ...m, mode: 'color' })) },
              ].map(item => (
                <div
                  key={item.label}
                  onClick={item.action}
                  style={{ padding: '9px 14px', fontSize: '13px', cursor: 'pointer', color: 'var(--color-text-primary)' }}
                  onMouseEnter={e => (e.currentTarget.style.background = 'var(--color-bg-hover)')}
                  onMouseLeave={e => (e.currentTarget.style.background = 'transparent')}
                >
                  {item.label}
                </div>
              ))}
            </>
          )}

          {ctxMenu.mode === 'font' && (
            <div style={{ padding: '12px', display: 'flex', flexDirection: 'column', gap: 10 }}>
              <div style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)' }}>改變字型</div>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                字型
                <select value={fontFamily} onChange={e => setFontFamily(e.target.value)}
                  style={{ padding: '4px 6px', borderRadius: 5, border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '12px' }}>
                  <option value="inherit">預設</option>
                  <option value="serif">Serif</option>
                  <option value="sans-serif">Sans-serif</option>
                  <option value="monospace">Monospace</option>
                  <option value="'Noto Serif TC', serif">Noto Serif TC</option>
                  <option value="'Source Han Sans TC', sans-serif">Source Han Sans</option>
                </select>
              </label>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                大小 (px)
                <input type="number" min={8} max={72} placeholder="例: 16" value={fontSize}
                  onChange={e => setFontSize(e.target.value)}
                  style={{ padding: '4px 6px', borderRadius: 5, border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '12px', width: '100%', boxSizing: 'border-box' }} />
              </label>
              <label style={{ display: 'flex', flexDirection: 'column', gap: 4, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                粗細
                <select value={fontWeight} onChange={e => setFontWeight(e.target.value)}
                  style={{ padding: '4px 6px', borderRadius: 5, border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '12px' }}>
                  <option value="inherit">預設</option>
                  <option value="300">細 (300)</option>
                  <option value="normal">正常 (400)</option>
                  <option value="500">中等 (500)</option>
                  <option value="bold">粗 (700)</option>
                  <option value="900">極粗 (900)</option>
                </select>
              </label>
              <div style={{ display: 'flex', gap: 6, marginTop: 2 }}>
                <button type="button" onClick={applyFont}
                  style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-accent)', color: 'white', fontSize: '12px', cursor: 'pointer' }}>
                  套用
                </button>
                <button type="button" onClick={() => setCtxMenu(m => m && ({ ...m, mode: 'menu' }))}
                  style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-bg-hover)', color: 'var(--color-text-secondary)', fontSize: '12px', cursor: 'pointer' }}>
                  返回
                </button>
              </div>
            </div>
          )}

          {ctxMenu.mode === 'color' && (
            <div style={{ padding: '12px', display: 'flex', flexDirection: 'column', gap: 10 }}>
              <div style={{ fontSize: '12px', fontWeight: 600, color: 'var(--color-text-secondary)' }}>改變顏色</div>
              <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
                {['#e03030','#e07830','#d4c020','#30a850','#2080e0','#8040e0','#e030a0','#888888','#000000'].map(c => (
                  <div key={c} onClick={() => setTextColor(c)}
                    style={{ width: 22, height: 22, borderRadius: 4, background: c, cursor: 'pointer',
                      border: textColor === c ? '2px solid var(--color-text-primary)' : '2px solid transparent' }} />
                ))}
              </div>
              <label style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: '12px', color: 'var(--color-text-muted)' }}>
                自訂
                <input type="color" value={textColor} onChange={e => setTextColor(e.target.value)}
                  style={{ width: 32, height: 24, padding: 0, border: '1px solid var(--color-border)', borderRadius: 4, cursor: 'pointer' }} />
                <span style={{ fontFamily: 'monospace' }}>{textColor}</span>
              </label>
              <div style={{ padding: '8px 10px', borderRadius: 6, background: 'var(--color-bg-base)', fontSize: '13px', border: '1px solid var(--color-border)' }}>
                <span style={{ color: textColor }}>{ctxMenu.text.slice(0, 30)}{ctxMenu.text.length > 30 ? '…' : ''}</span>
              </div>
              <div style={{ display: 'flex', gap: 6 }}>
                <button type="button" onClick={applyColor}
                  style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-accent)', color: 'white', fontSize: '12px', cursor: 'pointer' }}>
                  套用
                </button>
                <button type="button" onClick={() => setCtxMenu(m => m && ({ ...m, mode: 'menu' }))}
                  style={{ flex: 1, padding: '5px', borderRadius: 5, background: 'var(--color-bg-hover)', color: 'var(--color-text-secondary)', fontSize: '12px', cursor: 'pointer' }}>
                  返回
                </button>
              </div>
            </div>
          )}
        </div>
      )}

      {/* ── Edit Quick-Copy Modal ── */}
      {editQcModal && (
        <div style={{
          position: 'fixed', inset: 0, zIndex: 99999,
          background: 'rgba(0,0,0,0.45)', display: 'flex',
          alignItems: 'center', justifyContent: 'center',
        }} onMouseDown={() => setEditQcModal(null)}>
          <div style={{
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
            borderRadius: '10px', padding: '20px 24px', width: '360px',
            display: 'flex', flexDirection: 'column', gap: '12px',
          }} onMouseDown={(e) => e.stopPropagation()}>
            <div style={{ fontSize: '15px', fontWeight: 600, color: 'var(--color-text-primary)' }}>編輯快捷複製</div>

            <label style={{ display: 'flex', flexDirection: 'column', gap: '4px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字
              <input value={editQcModal.displayText}
                onChange={(e) => setEditQcModal(m => m && ({ ...m, displayText: e.target.value }))}
                style={{ padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
            </label>

            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字顏色
              <input type="color" value={editQcModal.color || '#2080e0'}
                onChange={(e) => setEditQcModal(m => m && ({ ...m, color: e.target.value }))}
                style={{ width: '36px', height: '28px', padding: '1px', border: '1px solid var(--color-border)', borderRadius: '4px', cursor: 'pointer', background: 'none' }} />
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: '12px', color: 'var(--color-text-muted)' }}>{editQcModal.color}</span>
            </label>

            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字大小（px）
              <input type="number" value={editQcModal.fontSize}
                onChange={(e) => setEditQcModal(m => m && ({ ...m, fontSize: e.target.value }))}
                placeholder="預設"
                style={{ width: '80px', padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }} />
            </label>

            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字字型
              <select value={editQcModal.fontFamily}
                onChange={(e) => setEditQcModal(m => m && ({ ...m, fontFamily: e.target.value }))}
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}>
                <option value="inherit">預設</option>
                <option value="serif">Serif</option>
                <option value="sans-serif">Sans-serif</option>
                <option value="monospace">Monospace</option>
              </select>
            </label>

            <label style={{ display: 'flex', alignItems: 'center', gap: '8px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              顯示文字粗細
              <select value={editQcModal.fontWeight}
                onChange={(e) => setEditQcModal(m => m && ({ ...m, fontWeight: e.target.value }))}
                style={{ flex: 1, padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px' }}>
                <option value="inherit">預設</option>
                <option value="normal">Normal</option>
                <option value="bold">Bold</option>
                <option value="600">Semi-bold (600)</option>
              </select>
            </label>

            <label style={{ display: 'flex', flexDirection: 'column', gap: '4px', fontSize: '13px', color: 'var(--color-text-secondary)' }}>
              可複製的內文
              <textarea value={editQcModal.copyContent}
                onChange={(e) => setEditQcModal(m => m && ({ ...m, copyContent: e.target.value }))}
                rows={3}
                style={{ padding: '5px 8px', borderRadius: '5px', border: '1px solid var(--color-border)', background: 'var(--color-bg-base)', color: 'var(--color-text-primary)', fontSize: '13px', resize: 'vertical', fontFamily: 'var(--font-mono)' }} />
            </label>

            <div style={{ fontSize: '12px', color: 'var(--color-text-muted)', display: 'flex', alignItems: 'center', gap: '8px' }}>
              預覽：
              <span className="quick-copy" style={{
                color: editQcModal.color || undefined,
                fontSize: editQcModal.fontSize ? `${editQcModal.fontSize}px` : undefined,
                fontFamily: editQcModal.fontFamily !== 'inherit' ? editQcModal.fontFamily : undefined,
                fontWeight: editQcModal.fontWeight !== 'inherit' ? editQcModal.fontWeight : undefined,
              }}>
                {editQcModal.displayText || editQcModal.copyContent || '範例文字'}
              </span>
            </div>

            <div style={{ display: 'flex', gap: '8px', justifyContent: 'flex-end', marginTop: '4px' }}>
              <button onClick={() => setEditQcModal(null)}
                style={{ padding: '6px 14px', borderRadius: '6px', fontSize: '13px', color: 'var(--color-text-secondary)', background: 'var(--color-bg-base)', border: '1px solid var(--color-border)' }}>
                取消
              </button>
              <button onClick={confirmEditQc}
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
