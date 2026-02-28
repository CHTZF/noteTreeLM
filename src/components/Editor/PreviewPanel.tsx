import { useEffect, useRef } from 'react'
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
  html: false,
  linkify: true,
  typographer: true,
}).use(mk, { engine: katex, delimiters: 'dollars', katexOptions: { throwOnError: false } })

interface PreviewPanelProps {
  content: string
  onWikilinkClick: (title: string) => void
  onEdit?: () => void
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
  // [[title|alias]] / [[title]] → <a>
  result = result.replace(/\[\[([^\[\]|#]+)(?:#[^\[\]|]*)?\|?([^\[\]]*)\]\]/g, (_, title, alias) => {
    const display = (alias || title).trim()
    return `<a href="#" data-wikilink="${title.trim()}" class="wikilink">${display}</a>`
  })
  return result
}

// 將圖片 src 解析為可用的圖片 URL
// - 絕對路徑 / vault 相對路徑 → 呼叫 Rust read_file_base64 回傳 data URL
// - src 可能被 markdown-it 的 encodeURI 編碼，先用 decodeURI 還原
async function resolveImageSrc(src: string, vaultPath: string): Promise<string> {
  if (src.startsWith('data:') || src.startsWith('http') || src.startsWith('vault:') || src.startsWith('asset:')) {
    return src
  }

  // markdown-it 會對 image URL 呼叫 encodeURI，此處還原為原始路徑
  let decoded = src
  try { decoded = decodeURI(src) } catch { /* 保持原值 */ }

  // 解析為絕對路徑
  const absolutePath = decoded.startsWith('/') ? decoded : `${vaultPath}/${decoded}`

  try {
    const base64 = await invoke<string>('read_file_base64', { path: absolutePath })
    return `data:${guessMimeType(absolutePath)};base64,${base64}`
  } catch (e) {
    console.warn('無法讀取圖片:', absolutePath, e)
    return src
  }
}

export default function PreviewPanel({ content, onWikilinkClick, onEdit }: PreviewPanelProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const { settings } = useSettingsStore()

  // preprocessImageUrls 先編碼空格，讓 markdown-it 能正確解析含空格的路徑
  // processWikilinks 在 md.render() 後注入 HTML，避免 html:false 轉義
  const html = DOMPurify.sanitize(
    processWikilinks(md.render(preprocessImageUrls(content))),
    { ADD_ATTR: ['data-wikilink', 'class', 'data-img-size'] }
  )

  // 圖片後處理：套用尺寸 + 解析圖片路徑為 data URL
  useEffect(() => {
    if (!containerRef.current) return
    const vaultPath = settings.vault_path

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
  }, [html, settings.vault_path])

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

    // 處理連結點擊
    const handleClick = (e: MouseEvent) => {
      const target = (e.target as HTMLElement).closest('a')
      if (!target) return
      e.preventDefault()

      const wikilink = target.getAttribute('data-wikilink')
      if (wikilink) {
        onWikilinkClick(wikilink)
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

  return (
    <div style={{ flex: 1, position: 'relative', display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
      {onEdit && (
        <button
          onClick={onEdit}
          title="開啟編輯器"
          style={{
            position: 'absolute', top: '10px', right: '14px', zIndex: 10,
            display: 'flex', alignItems: 'center', gap: '5px',
            padding: '5px 12px', borderRadius: '6px',
            background: 'var(--color-bg-elevated)', border: '1px solid var(--color-border)',
            color: 'var(--color-text-secondary)', fontSize: '12px',
            cursor: 'pointer', transition: 'all 0.15s',
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
      <div
        ref={containerRef}
        className="preview-content"
        dangerouslySetInnerHTML={{ __html: html }}
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
    </div>
  )
}
