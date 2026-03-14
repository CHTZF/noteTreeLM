import { ViewPlugin, ViewUpdate, Decoration, DecorationSet, WidgetType, EditorView } from '@codemirror/view'
import { syntaxTree } from '@codemirror/language'
import { RangeSetBuilder, StateField, Transaction, EditorState } from '@codemirror/state'
import type { Extension } from '@codemirror/state'
import type { SyntaxNodeRef } from '@lezer/common'
import MarkdownIt from 'markdown-it'
import mk from 'markdown-it-texmath'
import katex from 'katex'
import DOMPurify from 'dompurify'
import { invoke } from '@tauri-apps/api/core'
import { useSettingsStore } from '../../../stores/settingsStore'

// ── Shared markdown renderer ──────────────────────────────────────────────────
const mdBlock = new MarkdownIt({ html: true, linkify: true, typographer: true, breaks: true })
  .use(mk, { engine: katex, delimiters: 'dollars', katexOptions: { throwOnError: false } })

// ── Module-level EditorView ref — kept up-to-date by liveInlinePlugin ─────────
// Used by TableWidget to dispatch markdown changes on cell blur.
// Safe because there is only one editor view in this app at any time.
let _view: EditorView | null = null

// ── Module-level asset path cache (for Obsidian bare-filename fallback) ───────
// list_assets returns all vault file relative paths; cached for 30s to avoid
// repeated expensive scans when multiple images load simultaneously.
let _assetPaths: string[] = []
let _assetCacheTs = 0

// ── Table cell helpers ────────────────────────────────────────────────────────
function parseTableCells(line: string): string[] {
  let s = line.trim()
  if (s.startsWith('|')) s = s.slice(1)
  if (s.endsWith('|')) s = s.slice(0, -1)
  return s.split('|').map(c => c.trim())
}
function buildTableRow(cells: string[]): string {
  return '| ' + cells.join(' | ') + ' |'
}

// ── TableWidget: interactive contenteditable table ───────────────────────────
// Always renders the full table. Cells are contenteditable. On blur, the
// changed cell is written back to the markdown source via _view.dispatch.
class TableWidget extends WidgetType {
  constructor(
    private raw: string,       // markdown text of the table (no trailing \n)
    private tableFrom: number, // doc position of the first character of the table
  ) { super() }

  toDOM(): HTMLElement {
    const div = document.createElement('div')
    div.className = 'preview-content cm-live-block-widget'
    div.style.cssText = 'padding:0;margin:0;'
    try {
      const html = DOMPurify.sanitize(
        mdBlock.render(this.raw),
        { ADD_ATTR: ['class', 'style', 'data-copy'] }
      )
      div.innerHTML = html
      const table = div.querySelector('table')
      if (table) {
        const hdr = document.createElement('div')
        hdr.style.cssText = 'display:flex;justify-content:flex-end;padding:2px 4px;'
        const delBtn = document.createElement('button')
        delBtn.textContent = '×'
        delBtn.title = '刪除表格'
        delBtn.style.cssText = 'background:transparent;border:none;color:var(--color-text-muted);cursor:pointer;font-size:14px;padding:0 2px;'
        delBtn.addEventListener('mouseenter', () => { delBtn.style.color = 'var(--color-danger)' })
        delBtn.addEventListener('mouseleave', () => { delBtn.style.color = 'var(--color-text-muted)' })
        delBtn.addEventListener('click', () => this.deleteBlock())
        hdr.appendChild(delBtn)
        div.insertBefore(hdr, table)
        this.attachEditing(table)
      }
    } catch {
      div.textContent = this.raw
    }
    return div
  }

  private attachEditing(table: HTMLTableElement) {
    const rows = table.querySelectorAll('tr')
    rows.forEach((tr, rowIndex) => {
      tr.querySelectorAll('th, td').forEach((cell, colIndex) => {
        const el = cell as HTMLElement
        el.contentEditable = 'true'
        el.spellcheck = false
        el.style.outline = 'none'
        el.style.minWidth = '4ch'

        let originalContent = el.textContent ?? ''

        el.addEventListener('focus', () => {
          originalContent = el.textContent ?? ''
          el.style.background = 'var(--color-accent-dim)'
          el.style.outline = '1px solid var(--color-accent)'
          el.style.outlineOffset = '-1px'
        })

        el.addEventListener('blur', () => {
          el.style.background = ''
          el.style.outline = 'none'
          const newContent = (el.innerText ?? '').replace(/\n/g, ' ').trim()
          if (newContent !== originalContent.trim()) {
            this.applyChange(rowIndex, colIndex, newContent)
          }
        })

        el.addEventListener('keydown', (e: KeyboardEvent) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            el.blur()
          }
          if (e.key === 'Tab') {
            e.preventDefault()
            const allCells = Array.from(table.querySelectorAll('th, td'))
            const idx = allCells.indexOf(el)
            const next = allCells[e.shiftKey ? idx - 1 : idx + 1] as HTMLElement | undefined
            if (next) next.focus()
            else el.blur()
          }
          if (e.key === 'Escape') {
            e.preventDefault()
            el.textContent = originalContent  // revert
            el.blur()
          }
        })
      })
    })
  }

  private applyChange(rowIndex: number, colIndex: number, newContent: string) {
    if (!_view) return
    const doc = _view.state.doc
    const lines = this.raw.split('\n')
    const markdownLineIndex = rowIndex === 0 ? 0 : rowIndex + 1
    if (markdownLineIndex >= lines.length) return

    const oldLine = lines[markdownLineIndex]
    const cells = parseTableCells(oldLine)
    if (colIndex >= cells.length) return

    cells[colIndex] = newContent
    const newLine = buildTableRow(cells)
    if (newLine === oldLine) return

    // Resolve current position: try stored tableFrom first; fall back to full-doc search.
    // This handles the case where text was inserted before the table (shifting tableFrom).
    let tablePos = this.tableFrom
    if (doc.sliceString(tablePos, tablePos + this.raw.length) !== this.raw) {
      tablePos = doc.toString().indexOf(this.raw)
      if (tablePos < 0) return
    }

    const lineOffset = lines.slice(0, markdownLineIndex).reduce((s, l) => s + l.length + 1, 0)
    const lineFrom = tablePos + lineOffset
    const lineTo   = lineFrom + oldLine.length
    if (lineFrom < 0 || lineTo > doc.length) return

    _view.dispatch({ changes: { from: lineFrom, to: lineTo, insert: newLine } })
  }

  ignoreEvent(e: Event): boolean {
    return ['mousedown', 'mouseup', 'click',
            'keydown', 'keyup', 'keypress',
            'input', 'focus', 'blur',
            'compositionstart', 'compositionend',
            'paste', 'cut', 'copy'].includes(e.type)
  }

  private deleteBlock() {
    if (!_view) return
    const doc = _view.state.doc
    let pos = this.tableFrom
    if (doc.sliceString(pos, pos + this.raw.length) !== this.raw) {
      pos = doc.toString().indexOf(this.raw)
      if (pos < 0) return
    }
    const endPos = Math.min(pos + this.raw.length + 1, doc.length)
    _view.dispatch({ changes: { from: pos, to: endPos, insert: '' } })
  }

  // estimatedHeight: gives CM6 a good initial height estimate before DOM measurement,
  // preventing click-position offset during the first render cycle.
  get estimatedHeight(): number {
    // Visible rows = all markdown lines with '|', minus the delimiter row
    const visibleRows = Math.max(1, this.raw.split('\n').length - 1)
    return visibleRows * 37 + 8
  }

  // eq() does NOT compare tableFrom — position shifts are handled in applyChange.
  // This allows CM6 to reuse the widget DOM when only the position shifts
  // (e.g. text inserted before the table), keeping the height map stable.
  eq(other: WidgetType): boolean {
    return other instanceof TableWidget && (other as TableWidget).raw === this.raw
  }
}

// ── CodeWidget: interactive textarea-based code block ────────────────────────
// Always renders. User edits raw code in a styled textarea; on blur the fenced
// block is written back to the markdown source via _view.dispatch.
class CodeWidget extends WidgetType {
  constructor(
    private raw: string,       // full fenced code block markdown (no trailing \n)
    private codeFrom: number,  // doc position of first char (start of opening fence line)
  ) { super() }

  toDOM(): HTMLElement {
    const lines = this.raw.split('\n')
    const fence = lines[0]                        // e.g. "```rust" or "~~~"
    const lang  = fence.replace(/^[`~]+/, '').trim()
    const codeContent = lines.slice(1, -1).join('\n')  // between fences
    const closeFence  = lines[lines.length - 1]

    const wrapper = document.createElement('div')
    wrapper.className = 'preview-content cm-live-block-widget'
    wrapper.style.cssText = 'padding:0;margin:0 0 4px;border-radius:6px;overflow:hidden;border:1px solid var(--color-border);'

    // Header bar
    const header = document.createElement('div')
    header.style.cssText = 'display:flex;align-items:center;justify-content:space-between;padding:3px 10px;background:var(--color-bg-hover);border-bottom:1px solid var(--color-border);'
    const badge = document.createElement('span')
    badge.textContent = lang || 'code'
    badge.style.cssText = 'font-size:11px;font-family:var(--font-mono);color:var(--color-text-muted);'
    const delBtn = document.createElement('button')
    delBtn.textContent = '×'
    delBtn.title = '刪除程式碼區塊'
    delBtn.style.cssText = 'background:transparent;border:none;color:var(--color-text-muted);cursor:pointer;font-size:14px;padding:0 2px;'
    delBtn.addEventListener('mouseenter', () => { delBtn.style.color = 'var(--color-danger)' })
    delBtn.addEventListener('mouseleave', () => { delBtn.style.color = 'var(--color-text-muted)' })
    delBtn.addEventListener('click', () => this.deleteBlock())
    header.appendChild(badge)
    header.appendChild(delBtn)
    wrapper.appendChild(header)

    const textarea = document.createElement('textarea')
    textarea.value   = codeContent
    textarea.rows    = Math.max(1, lines.length - 2) + 1
    textarea.spellcheck = false
    textarea.style.cssText = [
      'width:100%;padding:10px 14px;',
      'font-family:var(--font-mono);font-size:0.88em;line-height:1.6;',
      'background:var(--color-bg-base);color:var(--color-text-primary);',
      'border:none;outline:none;resize:none;box-sizing:border-box;display:block;',
      'white-space:pre;overflow-x:auto;tab-size:2;',
    ].join('')

    let originalContent = codeContent

    textarea.addEventListener('focus', () => {
      originalContent = textarea.value
      wrapper.style.borderColor = 'var(--color-accent)'
    })
    textarea.addEventListener('blur', () => {
      wrapper.style.borderColor = 'var(--color-border)'
      if (textarea.value !== originalContent) {
        this.applyChange(textarea.value, fence, closeFence)
      }
    })
    textarea.addEventListener('input', () => {
      textarea.rows = Math.max(1, textarea.value.split('\n').length) + 1
    })
    textarea.addEventListener('keydown', (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault()
        textarea.value = originalContent
        textarea.blur()
      }
      if (e.key === 'Tab') {
        e.preventDefault()
        const { selectionStart: ss, selectionEnd: se } = textarea
        textarea.value = textarea.value.slice(0, ss) + '  ' + textarea.value.slice(se)
        textarea.selectionStart = textarea.selectionEnd = ss + 2
      }
      if (e.key === 'Backspace' && textarea.value === '') {
        e.preventDefault()
        this.deleteBlock()
      }
    })

    wrapper.appendChild(textarea)
    return wrapper
  }

  private applyChange(newContent: string, fence: string, closeFence: string) {
    if (!_view) return
    const newRaw = `${fence}\n${newContent}\n${closeFence}`
    if (newRaw === this.raw) return

    const doc = _view.state.doc
    // Resolve current position: try stored codeFrom first; fall back to full-doc search.
    let codePos = this.codeFrom
    if (doc.sliceString(codePos, codePos + this.raw.length) !== this.raw) {
      codePos = doc.toString().indexOf(this.raw)
      if (codePos < 0) return
    }
    if (codePos + this.raw.length > doc.length) return
    _view.dispatch({ changes: { from: codePos, to: codePos + this.raw.length, insert: newRaw } })
  }

  ignoreEvent(e: Event): boolean {
    return ['mousedown', 'mouseup', 'click',
            'keydown', 'keyup', 'keypress',
            'input', 'focus', 'blur',
            'compositionstart', 'compositionend',
            'paste', 'cut', 'copy'].includes(e.type)
  }

  private deleteBlock() {
    if (!_view) return
    const doc = _view.state.doc
    let pos = this.codeFrom
    if (doc.sliceString(pos, pos + this.raw.length) !== this.raw) {
      pos = doc.toString().indexOf(this.raw)
      if (pos < 0) return
    }
    const endPos = Math.min(pos + this.raw.length + 1, doc.length)
    _view.dispatch({ changes: { from: pos, to: endPos, insert: '' } })
  }

  get estimatedHeight(): number {
    const codeLines = Math.max(1, this.raw.split('\n').length - 2)
    // header(28) + wrapper-border(2) + bottom-margin(4) + textarea-padding(20) + lines
    return 28 + 2 + 4 + 20 + codeLines * 22
  }

  // eq() does NOT compare codeFrom — position shifts handled in applyChange.
  eq(other: WidgetType): boolean {
    return other instanceof CodeWidget && (other as CodeWidget).raw === this.raw
  }
}

// ── HR Widget ─────────────────────────────────────────────────────────────────
class HRWidget extends WidgetType {
  toDOM(): HTMLElement {
    const div = document.createElement('div')
    div.className = 'preview-content cm-live-block-widget'
    div.style.cssText = 'padding:0;margin:0;'
    div.innerHTML = '<hr>'
    return div
  }
  ignoreEvent(): boolean { return false }
  get estimatedHeight(): number { return 21 }
  eq(_: WidgetType): boolean { return true }
}
const hrWidget = new HRWidget()

// ── ImageWidget: renders ![alt](url) as an actual <img> element ───────────────
// For HTTP/data URLs, sets src synchronously.
// For local file paths, resolves asynchronously via Tauri read_file_base64.
class ImageWidget extends WidgetType {
  constructor(
    private src: string,
    private alt: string,
    private docFrom: number,
    private docTo: number,
  ) { super() }

  private deleteBlock() {
    if (!_view) return
    _view.dispatch({ changes: { from: this.docFrom, to: this.docTo, insert: '' } })
  }

  toDOM(): HTMLElement {
    // Parse optional |size from alt: "name|300" or "name|300x200"
    const barIdx = this.alt.lastIndexOf('|')
    const hasSize = barIdx !== -1 && /^\d/.test(this.alt.slice(barIdx + 1))
    const sizeStr = hasSize ? this.alt.slice(barIdx + 1) : ''
    let sizeW = '', sizeH = ''
    if (sizeStr) {
      const parts = sizeStr.split('x')
      sizeW = parts[0] ?? ''
      sizeH = parts[1] ?? ''
    }

    // Container
    const container = document.createElement('span')
    container.style.cssText = 'display:inline-block;position:relative;line-height:0;'
    container.addEventListener('contextmenu', (e) => {
      e.preventDefault()
      e.stopPropagation()
      _onLiveEditImage?.({ from: this.docFrom, to: this.docTo, src: this.src, alt: this.alt })
    })

    // Delete button
    const delBtn = document.createElement('button')
    delBtn.textContent = '×'
    delBtn.style.cssText = [
      'position:absolute;top:4px;right:4px;',
      'width:20px;height:20px;padding:0;border-radius:50%;',
      'background:rgba(0,0,0,0.55);color:#fff;border:none;',
      'font-size:14px;line-height:1;cursor:pointer;',
      'opacity:0;transition:opacity 0.15s;z-index:10;',
    ].join('')
    container.addEventListener('mouseenter', () => { delBtn.style.opacity = '1' })
    container.addEventListener('mouseleave', () => { delBtn.style.opacity = '0' })
    delBtn.addEventListener('mousedown', (e) => { e.preventDefault(); e.stopPropagation() })
    delBtn.addEventListener('click', (e) => { e.preventDefault(); e.stopPropagation(); this.deleteBlock() })

    const img = document.createElement('img')
    img.alt = hasSize ? this.alt.slice(0, barIdx) : this.alt
    img.style.cssText = 'border-radius:var(--radius-md);vertical-align:middle;cursor:default;display:block;'
    if (sizeW) img.style.width = `${sizeW}px`
    if (sizeH) img.style.height = `${sizeH}px`
    if (!sizeW && !sizeH) img.style.maxWidth = '100%'

    const loadLocal = (rawSrc: string) => {
      const { settings } = useSettingsStore.getState()
      // Normalize backslashes to forward slashes for Windows vault paths
      const vaultPath = settings.system_current_vault_path.replace(/\\/g, '/')
      let decoded = rawSrc
      try { decoded = decodeURI(rawSrc) } catch { /* keep original */ }
      // Detect absolute paths: Unix (/...) and Windows (C:\... or C:/...)
      const isAbsolute = decoded.startsWith('/') || /^[A-Za-z]:[/\\]/.test(decoded)
      const absolutePath = isAbsolute ? decoded : `${vaultPath}/${decoded}`

      const mimeMap: Record<string, string> = {
        png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg',
        gif: 'image/gif', webp: 'image/webp', svg: 'image/svg+xml',
        bmp: 'image/bmp', ico: 'image/x-icon',
      }
      const applyBase64 = (path: string, base64: string) => {
        const ext = path.split('.').pop()?.toLowerCase() ?? ''
        img.src = `data:${mimeMap[ext] ?? 'image/png'};base64,${base64}`
      }
      const showError = () => {
        img.title = `無法載入: ${decoded}`
        img.style.border = '1px dashed var(--color-border)'
        img.style.padding = '4px'
        img.style.minWidth = '60px'
        img.style.minHeight = '24px'
      }

      invoke<string>('read_file_base64', { path: absolutePath })
        .then(base64 => applyBase64(absolutePath, base64))
        .catch(async () => {
          // Obsidian compatibility: bare filename (no path separator) → search entire vault
          const isBareFilename = !isAbsolute && !decoded.includes('/') && !decoded.includes('\\')
          if (isBareFilename) {
            if (Date.now() - _assetCacheTs > 30000) {
              try { _assetPaths = await invoke<string[]>('list_assets'); _assetCacheTs = Date.now() } catch { /* keep cache */ }
            }
            const lower = decoded.toLowerCase()
            const match = _assetPaths.find(a => a.toLowerCase() === lower || a.toLowerCase().endsWith('/' + lower))
            if (match) {
              const matchAbsPath = `${vaultPath}/${match}`
              invoke<string>('read_file_base64', { path: matchAbsPath })
                .then(base64 => applyBase64(matchAbsPath, base64))
                .catch(showError)
              return
            }
          }
          showError()
        })
    }

    if (this.src.startsWith('http') || this.src.startsWith('data:') ||
        this.src.startsWith('asset:') || this.src.startsWith('vault:')) {
      img.src = this.src
    } else {
      loadLocal(this.src)
    }

    container.appendChild(img)
    container.appendChild(delBtn)
    return container
  }

  ignoreEvent(e: Event): boolean {
    return ['contextmenu', 'mousedown', 'mouseup', 'click'].includes(e.type)
  }

  eq(other: WidgetType): boolean {
    return other instanceof ImageWidget &&
      (other as ImageWidget).src === this.src &&
      (other as ImageWidget).alt === this.alt
  }
}

// ── Image right-click edit callback ──────────────────────────────────────────
export interface ImageEditData { from: number; to: number; src: string; alt: string }
let _onLiveEditImage: ((data: ImageEditData) => void) | null = null
export function setLiveEditImageHandler(fn: typeof _onLiveEditImage) {
  _onLiveEditImage = fn
}

// ── QuickCopy right-click edit callback ──────────────────────────────────────
// Editor.tsx registers this so live-mode right-click on a span opens the modal.
export interface QuickCopyEditData {
  from: number; to: number
  dataCopy: string; displayText: string; style: string
}
let _onLiveEditQuickCopy: ((data: QuickCopyEditData) => void) | null = null
export function setLiveEditQuickCopyHandler(fn: typeof _onLiveEditQuickCopy) {
  _onLiveEditQuickCopy = fn
}

// ── QuickCopyWidget: renders <span class="quick-copy" ...> with live styling ──
// Always rendered (no cursorIn toggle). Left-click copies; right-click opens editor.
class QuickCopyWidget extends WidgetType {
  constructor(
    private dataCopy: string,
    private displayText: string,
    private style: string,
    private docFrom: number,
    private docTo: number,
  ) { super() }

  private deleteBlock() {
    if (!_view) return
    _view.dispatch({ changes: { from: this.docFrom, to: this.docTo, insert: '' } })
  }

  toDOM(): HTMLElement {
    const container = document.createElement('span')
    container.style.cssText = 'display:inline-flex;align-items:center;gap:2px;'

    const span = document.createElement('span')
    span.className = 'quick-copy'
    span.setAttribute('data-copy', this.dataCopy)
    span.textContent = this.displayText
    if (this.style) span.setAttribute('style', this.style)
    span.addEventListener('click', () => {
      navigator.clipboard.writeText(this.dataCopy).catch(() => {})
      span.style.opacity = '0.4'
      setTimeout(() => { span.style.opacity = '' }, 800)
    })
    span.addEventListener('contextmenu', (e) => {
      e.preventDefault()
      e.stopPropagation()
      _onLiveEditQuickCopy?.({
        from: this.docFrom, to: this.docTo,
        dataCopy: this.dataCopy, displayText: this.displayText, style: this.style,
      })
    })

    const delBtn = document.createElement('button')
    delBtn.textContent = '×'
    delBtn.style.cssText = [
      'width:16px;height:16px;padding:0;border-radius:50%;',
      'background:var(--color-border);color:var(--color-text-muted);border:none;',
      'font-size:12px;line-height:1;cursor:pointer;',
      'opacity:0;transition:opacity 0.15s;flex-shrink:0;',
    ].join('')
    container.addEventListener('mouseenter', () => { delBtn.style.opacity = '1' })
    container.addEventListener('mouseleave', () => { delBtn.style.opacity = '0' })
    delBtn.addEventListener('mousedown', (e) => { e.preventDefault(); e.stopPropagation() })
    delBtn.addEventListener('click', (e) => { e.preventDefault(); e.stopPropagation(); this.deleteBlock() })

    container.appendChild(span)
    container.appendChild(delBtn)
    return container
  }

  ignoreEvent(e: Event): boolean {
    return ['click', 'mousedown', 'mouseup', 'contextmenu'].includes(e.type)
  }

  eq(other: WidgetType): boolean {
    return other instanceof QuickCopyWidget &&
      (other as QuickCopyWidget).dataCopy === this.dataCopy &&
      (other as QuickCopyWidget).displayText === this.displayText &&
      (other as QuickCopyWidget).style === this.style
  }
}

type DecoEntry = { from: number; to: number; deco: Decoration }

// ── Block decoration builder ──────────────────────────────────────────────────
// NOTE: no cursor/selection dependency — block widgets are always shown.
// This allows CM6 to reuse widget DOM nodes across cursor movements and keep
// accurate height measurements, preventing the "cursor lands 2 lines below
// click" issue caused by repeated widget recreation + height re-estimation.
function buildBlockDecos(state: EditorState): DecorationSet {
  const doc   = state.doc
  const decos: DecoEntry[] = []

  syntaxTree(state).iterate({
    enter(node: SyntaxNodeRef) {
      const { from, to, name } = node

      // ── Table ─────────────────────────────────────────────────────────
      if (name === 'Table') {
        const tableFirstLine = doc.lineAt(from)
        const effTo          = to > from && doc.lineAt(to).from === to ? to - 1 : to
        const tableLastLine  = doc.lineAt(effTo)
        const tableBlockTo   = tableLastLine.to < doc.length ? tableLastLine.to + 1 : doc.length
        const raw = doc.sliceString(tableFirstLine.from, tableLastLine.to)
        decos.push({
          from: tableFirstLine.from, to: tableBlockTo,
          deco: Decoration.replace({ widget: new TableWidget(raw, tableFirstLine.from), block: true }),
        })
        return false
      }

      // ── FencedCode ────────────────────────────────────────────────────
      if (name === 'FencedCode') {
        const fenceFirstLine = doc.lineAt(from)
        const effTo          = to > from && doc.lineAt(to).from === to ? to - 1 : to
        const fenceLastLine  = doc.lineAt(effTo)
        const fenceBlockTo   = fenceLastLine.to < doc.length ? fenceLastLine.to + 1 : doc.length
        const raw = doc.sliceString(fenceFirstLine.from, fenceLastLine.to)
        decos.push({
          from: fenceFirstLine.from, to: fenceBlockTo,
          deco: Decoration.replace({ widget: new CodeWidget(raw, fenceFirstLine.from), block: true }),
        })
        return false
      }

      // ── HorizontalRule ────────────────────────────────────────────────
      if (name === 'HorizontalRule') {
        const hrFromLine = doc.lineAt(from)
        const hrLastPos  = (to > from && doc.lineAt(to).from === to) ? to - 1 : to
        const hrToLine   = doc.lineAt(hrLastPos)
        const hrFrom = hrFromLine.from
        const hrTo   = hrToLine.to < doc.length ? hrToLine.to + 1 : doc.length
        if (hrFrom < hrTo) {
          decos.push({ from: hrFrom, to: hrTo, deco: Decoration.replace({ widget: hrWidget, block: true }) })
        }
        return false
      }
    },
  })

  decos.sort((a, b) => a.from !== b.from ? a.from - b.from : b.to - a.to)
  const builder = new RangeSetBuilder<Decoration>()
  for (const { from, to, deco } of decos) {
    try { builder.add(from, to, deco) } catch { /* skip */ }
  }
  return builder.finish()
}

// ── StateField for block decorations ─────────────────────────────────────────
// Only rebuild on docChanged — NOT on selection/cursor movement.
// This keeps CM6's height map stable across cursor movements, fixing the
// click-position offset caused by widget height re-estimation.
const liveBlockField = StateField.define<DecorationSet>({
  create(state) {
    try { return buildBlockDecos(state) } catch { return Decoration.none }
  },
  update(decos, tr: Transaction) {
    if (tr.docChanged) {
      try { return buildBlockDecos(tr.state) } catch { return Decoration.none }
    }
    return decos.map(tr.changes)
  },
  provide: f => EditorView.decorations.from(f),
})

// ── Inline decoration builder ─────────────────────────────────────────────────
function buildInlineDecos(view: EditorView): DecorationSet {
  const cursor = view.state.selection.main.head
  const doc    = view.state.doc
  const decos: DecoEntry[] = []

  // lineDecoMap: lineFrom → [class names]; deduplicated by line position
  const lineDecoMap = new Map<number, string[]>()
  const addLine = (lineFrom: number, cls: string) => {
    const arr = lineDecoMap.get(lineFrom) ?? []
    arr.push(cls)
    lineDecoMap.set(lineFrom, arr)
  }

  const add     = (from: number, to: number, deco: Decoration) => { if (from < to) decos.push({ from, to, deco }) }
  const addMark = (from: number, to: number, cls: string) => add(from, to, Decoration.mark({ class: cls }))
  const cursorIn = (from: number, to: number) => cursor >= from && cursor <= to

  for (const { from: vpFrom, to: vpTo } of view.visibleRanges) {
    syntaxTree(view.state).iterate({
      from: vpFrom,
      to: vpTo,
      enter(node: SyntaxNodeRef) {
        const { from, to, name } = node

        // Block elements handled by StateField
        if (name === 'Table' || name === 'FencedCode' || name === 'HorizontalRule') return false

        // ── ATX Headings ──────────────────────────────────────────────
        if (name.startsWith('ATXHeading')) {
          const level = parseInt(name.slice(10))
          if (!cursorIn(from, to)) {
            let markEnd = from
            let child = node.node.firstChild
            while (child) {
              if (child.name === 'HeaderMark') {
                markEnd = child.to
                if (doc.sliceString(markEnd, markEnd + 1) === ' ') markEnd++
                break
              }
              child = child.nextSibling
            }
            if (markEnd > from) addMark(from, markEnd, 'cm-live-hidden')
            if (markEnd < to)   addMark(markEnd, to, `cm-live-h${level}`)
          }
          return false
        }

        // ── StrongEmphasis **bold** ────────────────────────────────────
        if (name === 'StrongEmphasis') {
          if (!cursorIn(from, to)) {
            const first = node.node.firstChild
            const last  = node.node.lastChild
            if (first?.name === 'EmphasisMark' && last?.name === 'EmphasisMark' && first !== last) {
              addMark(first.from, first.to, 'cm-live-hidden')
              addMark(first.to, last.from, 'cm-live-bold')
              addMark(last.from, last.to, 'cm-live-hidden')
            }
          }
        }

        // ── Emphasis *italic* ──────────────────────────────────────────
        if (name === 'Emphasis') {
          if (!cursorIn(from, to)) {
            const first = node.node.firstChild
            const last  = node.node.lastChild
            if (first?.name === 'EmphasisMark' && last?.name === 'EmphasisMark' && first !== last) {
              addMark(first.from, first.to, 'cm-live-hidden')
              addMark(first.to, last.from, 'cm-live-italic')
              addMark(last.from, last.to, 'cm-live-hidden')
            }
          }
        }

        // ── Strikethrough ~~text~~ ─────────────────────────────────────
        if (name === 'Strikethrough') {
          if (!cursorIn(from, to)) {
            const first = node.node.firstChild
            const last  = node.node.lastChild
            if (first?.name === 'StrikethroughMark' && last?.name === 'StrikethroughMark' && first !== last) {
              addMark(first.from, first.to, 'cm-live-hidden')
              addMark(first.to, last.from, 'cm-live-strike')
              addMark(last.from, last.to, 'cm-live-hidden')
            }
          }
          return false
        }

        // ── InlineCode `code` ──────────────────────────────────────────
        if (name === 'InlineCode') {
          if (!cursorIn(from, to)) {
            let openMark: { from: number; to: number } | null = null
            let closeMark: { from: number; to: number } | null = null
            let child = node.node.firstChild
            while (child) {
              if (child.name === 'CodeMark') {
                if (!openMark) openMark = { from: child.from, to: child.to }
                else closeMark = { from: child.from, to: child.to }
              }
              child = child.nextSibling
            }
            if (openMark && closeMark) {
              addMark(openMark.from, openMark.to, 'cm-live-hidden')
              addMark(openMark.to, closeMark.from, 'cm-live-code')
              addMark(closeMark.from, closeMark.to, 'cm-live-hidden')
            }
          }
          return false
        }

        // ── Blockquote line styling ────────────────────────────────────
        if (name === 'Blockquote') {
          const effTo = to > from && doc.lineAt(to).from === to ? to - 1 : to
          const startLine = doc.lineAt(from)
          const endLine   = doc.lineAt(effTo)
          for (let n = startLine.number; n <= endLine.number; n++) {
            addLine(doc.line(n).from, 'cm-live-blockquote-line')
          }
          // Don't return false — QuoteMark inside will hide '>'
        }

        // ── List item styling ──────────────────────────────────────────
        if (name === 'ListItem') {
          const isOrdered = node.node.parent?.name === 'OrderedList'
          addLine(doc.lineAt(from).from, isOrdered ? 'cm-live-ol-item' : 'cm-live-ul-item')
          if (!isOrdered && !cursorIn(from, to)) {
            let child = node.node.firstChild
            while (child) {
              if (child.name === 'ListMark') {
                addMark(child.from, child.to, 'cm-live-hidden')
                break
              }
              child = child.nextSibling
            }
          }
        }

        // ── Blockquote > text ──────────────────────────────────────────
        if (name === 'QuoteMark') {
          const parentFrom = node.node.parent?.from ?? from
          const parentTo   = node.node.parent?.to   ?? to
          if (!cursorIn(parentFrom, parentTo)) {
            let end = to
            if (doc.sliceString(end, end + 1) === ' ') end++
            addMark(from, end, 'cm-live-hidden')
          }
          return false
        }

        // ── Link [text](url) ──────────────────────────────────────────
        if (name === 'Link') {
          if (!cursorIn(from, to)) {
            let textStart = from
            let textEnd   = to
            let markCount = 0
            let child = node.node.firstChild
            while (child) {
              if (child.name === 'LinkMark') {
                markCount++
                if (markCount === 1) {
                  addMark(child.from, child.to, 'cm-live-hidden')
                  textStart = child.to
                } else if (markCount === 2) {
                  textEnd = child.from
                  addMark(child.from, child.to, 'cm-live-hidden')
                } else {
                  addMark(child.from, child.to, 'cm-live-hidden')
                }
              } else if (child.name === 'URL' || child.name === 'LinkTitle') {
                addMark(child.from, child.to, 'cm-live-hidden')
              }
              child = child.nextSibling
            }
            if (textStart < textEnd) addMark(textStart, textEnd, 'cm-live-link')
          }
          return false
        }
      },
    })
  }

  // ── Image syntax scan ─────────────────────────────────────────────────────
  // Regex-based (more reliable than syntax tree for ![alt|size](url) variants).
  const imageRe = /!\[([^\]]*)\]\(([^)]+)\)/g
  for (const { from: vpFrom, to: vpTo } of view.visibleRanges) {
    const text = doc.sliceString(vpFrom, vpTo)
    let m: RegExpExecArray | null
    imageRe.lastIndex = 0
    while ((m = imageRe.exec(text)) !== null) {
      const mFrom = vpFrom + m.index
      const mTo = mFrom + m[0].length
      const src = m[2].trim()
      if (src) add(mFrom, mTo, Decoration.replace({ widget: new ImageWidget(src, m[1], mFrom, mTo) }))
    }
  }

  // ── Obsidian-style vault embed: ![[path]] or ![[path|size]] ───────────────
  const vaultImgRe = /!\[\[([^\]]+)\]\]/g
  for (const { from: vpFrom, to: vpTo } of view.visibleRanges) {
    const text = doc.sliceString(vpFrom, vpTo)
    let m: RegExpExecArray | null
    vaultImgRe.lastIndex = 0
    while ((m = vaultImgRe.exec(text)) !== null) {
      const inner = m[1] // e.g. "assets/photo.png" or "assets/photo.png|300"
      const parts = inner.split('|')
      const relPath = parts[0].trim()
      const ext = relPath.split('.').pop()?.toLowerCase() ?? ''
      if (!['png','jpg','jpeg','gif','webp','svg','bmp','avif'].includes(ext)) continue
      const mFrom = vpFrom + m.index
      const mTo = mFrom + m[0].length
      // Use relative path directly — ImageWidget.loadLocal() will resolve it
      // via read_file_base64, which is reliable on all platforms (avoids vault://
      // protocol issues on Windows WebView2).
      const suffix = parts.slice(1).join('|') // e.g. "300" or "name|300" or ""
      // ImageWidget parses alt as "name|size" — if suffix starts with a digit it's a bare
      // size (no name), so prepend | so the last-|index logic picks it up correctly.
      const alt = suffix && /^\d/.test(suffix) ? `|${suffix}` : suffix
      add(mFrom, mTo, Decoration.replace({ widget: new ImageWidget(relPath, alt, mFrom, mTo) }))
    }
  }

  // ── Quick-copy HTML spans ──────────────────────────────────────────────────
  // Always rendered (no cursorIn check). Right-click opens the edit modal.
  const quickCopyRe = /<span class="quick-copy" data-copy="([^"]*)"([^>]*)>(.*?)<\/span>/g
  for (const { from: vpFrom, to: vpTo } of view.visibleRanges) {
    const text = doc.sliceString(vpFrom, vpTo)
    let m: RegExpExecArray | null
    quickCopyRe.lastIndex = 0
    while ((m = quickCopyRe.exec(text)) !== null) {
      const mFrom = vpFrom + m.index
      const mTo = mFrom + m[0].length
      const dataCopy = m[1].replace(/&quot;/g, '"').replace(/&amp;/g, '&')
      const styleMatch = m[2].match(/style="([^"]*)"/)
      const style = styleMatch ? styleMatch[1] : ''
      add(mFrom, mTo, Decoration.replace({ widget: new QuickCopyWidget(dataCopy, m[3], style, mFrom, mTo) }))
    }
  }

  decos.sort((a, b) => a.from !== b.from ? a.from - b.from : b.to - a.to)
  // Build sorted line-decoration entries (one per line, merging multiple classes)
  const lineDecoArr: DecoEntry[] = Array.from(lineDecoMap.entries())
    .map(([lf, classes]) => ({ from: lf, to: lf, deco: Decoration.line({ class: classes.join(' ') }) }))
    .sort((a, b) => a.from - b.from)
  // Merge: line decos before marks at the same position (CM6 requirement)
  const allDecos: DecoEntry[] = []
  let di = 0, li = 0
  while (di < decos.length || li < lineDecoArr.length) {
    const d = decos[di], l = lineDecoArr[li]
    if (!d)                   { allDecos.push(l); li++ }
    else if (!l)              { allDecos.push(d); di++ }
    else if (l.from < d.from) { allDecos.push(l); li++ }
    else if (l.from > d.from) { allDecos.push(d); di++ }
    else                      { allDecos.push(l); li++ } // same from → line deco first
  }
  const builder = new RangeSetBuilder<Decoration>()
  for (const { from, to, deco } of allDecos) {
    try { builder.add(from, to, deco) } catch { /* skip */ }
  }
  return builder.finish()
}

// ── ViewPlugin for inline decorations + maintaining _view reference ───────────
const liveInlinePlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet
    constructor(view: EditorView) {
      _view = view
      try { this.decorations = buildInlineDecos(view) } catch { this.decorations = Decoration.none }
    }
    update(update: ViewUpdate) {
      _view = update.view
      if (update.docChanged || update.selectionSet || update.viewportChanged) {
        try { this.decorations = buildInlineDecos(update.view) } catch { this.decorations = Decoration.none }
      }
    }
    destroy() { _view = null }
  },
  { decorations: (v) => v.decorations },
)

// ── Export ────────────────────────────────────────────────────────────────────
export const livePreviewPlugin: Extension = [liveBlockField, liveInlinePlugin]

// ── Theme (empty) ─────────────────────────────────────────────────────────────
// All cm-live-* styles have been moved to App.css (global CSS) to avoid
// WebView2 CSP issues with dynamically-injected Constructable Stylesheets on Windows.
// The export is kept so Editor.tsx doesn't need to change its import.
export const livePreviewTheme = EditorView.baseTheme({})
