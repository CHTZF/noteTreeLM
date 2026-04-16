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

// ── Table column width cache (survives widget re-renders) ─────────────────────
// Key: first line of the table markdown (header row) — stable across data edits.
// Value: array of px widths per column index.
// Exported so PreviewPanel can apply the same widths to its rendered tables.
export const tableColWidths = new Map<string, number[]>()
const _tableColWidths = tableColWidths

// ── Custom style syntax → HTML (shared with PreviewPanel) ────────────────────
// ── 新樣式語法：{style:key=val,key=val}text{/style} ─────────────────────────
// 完全扁平，無巢狀。key 可以是：color, fontFamily, fontSize, fontWeight
// 例：{style:color=#e03030,fontSize=14,fontFamily=Arial,fontWeight=bold}hello{/style}

type StyleProps = Record<string, string>

function parseStyleParams(params: string): StyleProps {
  const result: StyleProps = {}
  params.split(',').forEach(pair => {
    const eq = pair.indexOf('=')
    if (eq < 0) return
    const k = pair.slice(0, eq).trim()
    const v = pair.slice(eq + 1).trim()
    if (k && v) result[k] = v
  })
  return result
}

function buildStyleParams(props: StyleProps): string {
  return Object.entries(props).map(([k, v]) => `${k}=${v}`).join(',')
}

function propsToCSS(props: StyleProps): string {
  const parts: string[] = []
  if (props.color) parts.push(`color:${props.color}`)
  if (props.fontFamily && props.fontFamily !== 'inherit') {
    const f = props.fontFamily
    parts.push(`font-family:${f.includes(' ') ? `'${f}'` : f}`)
  }
  if (props.fontSize) parts.push(`font-size:${props.fontSize}px`)
  if (props.fontWeight && props.fontWeight !== 'inherit') parts.push(`font-weight:${props.fontWeight}`)
  return parts.join(';')
}

// renderCustomStyle: {style:...}text{/style} → <span style="..." data-style-params="...">
function renderCustomStyle(text: string): string {
  const re = /\{style:([^}\n]+)\}(.*?)\{\/style\}/g
  return text.replace(re, (_, params, inner) => {
    const props = parseStyleParams(params)
    const css = propsToCSS(props)
    return css
      ? `<span style="${css}" data-style-params="${params}">${inner}</span>`
      : inner
  })
}

// cellHtmlToRaw: span innerHTML → markdown（利用 data-style-params）
function cellHtmlToRaw(container: HTMLElement): string {
  function walk(node: Node): string {
    if (node.nodeType === Node.TEXT_NODE) return node.textContent ?? ''
    const el = node as HTMLElement
    const children = Array.from(el.childNodes).map(walk).join('')
    if (el === container) return children
    const params = el.getAttribute('data-style-params')
    if (params) return `{style:${params}}${children}{/style}`
    return children
  }
  return walk(container).replace(/\n/g, ' ').trim()
}

// computeDomOffset: Range 端點 → cellSpan plain text 字元偏移
function computeDomOffset(cellSpan: HTMLElement, targetNode: Node, targetOffset: number): number {
  try {
    const r = document.createRange()
    r.setStart(cellSpan, 0)
    r.setEnd(targetNode, targetOffset)
    return r.toString().length
  } catch {
    return (cellSpan.textContent ?? '').length
  }
}

// ── Segment-based 樣式套用 ────────────────────────────────────────────────────
// cellRaw 的結構永遠是：plain text 和 {style:...}text{/style} 的序列（無巢狀）
// applyStyleToRange: 在 plain text 偏移 [domStart, domEnd] 套上新樣式，並 merge 既有 props

interface CellSegment { text: string; props: StyleProps | null }

function parseSegments(cellRaw: string): CellSegment[] {
  const segs: CellSegment[] = []
  const re = /\{style:([^}]+)\}(.*?)\{\/style\}/g
  let last = 0, m: RegExpExecArray | null
  while ((m = re.exec(cellRaw)) !== null) {
    if (m.index > last) segs.push({ text: cellRaw.slice(last, m.index), props: null })
    segs.push({ text: m[2], props: parseStyleParams(m[1]) })
    last = m.index + m[0].length
  }
  if (last < cellRaw.length) segs.push({ text: cellRaw.slice(last), props: null })
  return segs
}

function serializeSegments(segs: CellSegment[]): string {
  return segs.map(s => {
    if (!s.props || Object.keys(s.props).length === 0) return s.text
    return `{style:${buildStyleParams(s.props)}}${s.text}{/style}`
  }).join('')
}

function applyStyleToRange(cellRaw: string, domStart: number, domEnd: number, newProps: StyleProps): string {
  if (domEnd <= domStart) return cellRaw
  const segs = parseSegments(cellRaw)
  const newSegs: CellSegment[] = []
  let offset = 0
  for (const seg of segs) {
    const segStart = offset
    const segEnd = offset + seg.text.length
    offset = segEnd
    if (segEnd <= domStart || segStart >= domEnd) { newSegs.push(seg); continue }
    const relStart = Math.max(segStart, domStart) - segStart
    const relEnd   = Math.min(segEnd,   domEnd)   - segStart
    const before   = seg.text.slice(0, relStart)
    const selected = seg.text.slice(relStart, relEnd)
    const after    = seg.text.slice(relEnd)
    const merged   = { ...(seg.props ?? {}), ...newProps }
    if (before)   newSegs.push({ text: before,   props: seg.props })
    if (selected) newSegs.push({ text: selected, props: merged })
    if (after)    newSegs.push({ text: after,    props: seg.props })
  }
  return serializeSegments(newSegs)
}

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
    private raw: string,            // markdown text of the table (no trailing \n)
    private tableFrom: number,      // doc position of the first character of the table
    private colWidthsLine = '',     // e.g. "<!-- col-widths: 120,200,80 -->" or ''
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

        // 把儲存格文字包在 <span contenteditable> 裡，th/td 本身不設 contenteditable。
        // 這樣 resize handle（絕對定位的 div，th 的另一個子元素）不會讓 WebKit
        // 誤算可輸入寬度，避免「輸入一個字就換行」的問題。
        const rawText = el.textContent ?? ''
        el.textContent = ''

        const span = document.createElement('span')
        span.contentEditable = 'true'
        span.spellcheck = false
        span.style.cssText = 'display:block;outline:none;min-width:0;word-break:break-word;padding-right:6px;'
        el.appendChild(span)

        // cellRaw 追蹤真正的 markdown 原始文字（含 {color:}/{font:} 語法）
        let cellRaw = rawText
        const renderCell = () => {
          span.innerHTML = DOMPurify.sanitize(
            renderCustomStyle(cellRaw),
            { ALLOWED_TAGS: ['span'], ADD_ATTR: ['style', 'data-style-params'] }
          )
        }
        renderCell()

        span.addEventListener('focus', () => {
          // 保持樣式化 HTML，直接在 WYSIWYG 狀態下編輯
          el.style.background = 'var(--color-accent-dim)'
          el.style.outline = '1px solid var(--color-accent)'
          el.style.outlineOffset = '-1px'
        })

        span.addEventListener('blur', () => {
          el.style.background = ''
          el.style.outline = 'none'
          // 從 DOM 反向還原 markdown 語法（利用 data-style-params）
          const newContent = cellHtmlToRaw(span)
          const changed = newContent !== cellRaw
          if (changed) {
            cellRaw = newContent
            this.applyChange(rowIndex, colIndex, newContent)
          }
          // 重新 render（確保樣式正確，並補上可能遺失的 data 屬性）
          renderCell()
        })

        span.addEventListener('keydown', (e: KeyboardEvent) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            span.blur()
          }
          if (e.key === 'Tab') {
            e.preventDefault()
            const allCells = Array.from(table.querySelectorAll('th, td'))
            const idx = allCells.indexOf(el)
            const nextCell = allCells[e.shiftKey ? idx - 1 : idx + 1] as HTMLElement | undefined
            if (nextCell) (nextCell.querySelector('span[contenteditable]') as HTMLElement | null)?.focus()
            else span.blur()
          }
          if (e.key === 'Escape') {
            e.preventDefault()
            renderCell()  // 還原原始內容
            span.blur()
          }
        })

        // ── 右鍵選單 ──────────────────────────────────────────────────────────
        el.addEventListener('contextmenu', (e: MouseEvent) => {
          e.preventDefault()
          e.stopPropagation()
          const sel = window.getSelection()
          const selText = sel?.toString() ?? ''
          const range = sel && sel.rangeCount > 0 ? sel.getRangeAt(0) : null
          const savedSel = (selText && range && span.contains(range.startContainer))
            ? {
                text: selText,
                domStart: computeDomOffset(span, range.startContainer, range.startOffset),
                domEnd:   computeDomOffset(span, range.endContainer,   range.endOffset),
                currentRaw: () => cellRaw,
              }
            : null
          const onApplyFormat = (newContent: string) => {
            cellRaw = newContent
            renderCell()
            this.applyChange(rowIndex, colIndex, newContent)
          }
          this.showContextMenu(e.clientX, e.clientY, rowIndex, colIndex, span, savedSel, onApplyFormat)
        })
      })
    })

    // ── 欄寬調整（在 editing 迴圈之後，handle 才不會被 textContent='' 清掉）
    this.attachColResize(table)
  }

  // ── 欄寬 resize handles ────────────────────────────────────────────────────
  private attachColResize(table: HTMLTableElement) {
    const cacheKey = this.raw.split('\n')[0] // header row — stable across data edits

    const firstRow = table.querySelector('tr')
    const headerCells = firstRow ? Array.from(firstRow.querySelectorAll('th, td')) as HTMLElement[] : []
    if (!headerCells.length) return

    const saveWidths = () => {
      const widths = headerCells.map(hc => hc.offsetWidth)
      _tableColWidths.set(cacheKey, widths)
      // 把欄寬寫回 markdown 的 col-widths 注釋
      if (!_view) return
      const newComment = `<!-- col-widths: ${widths.join(',')} -->`
      const doc = _view.state.doc
      let tablePos = this.tableFrom
      if (doc.sliceString(tablePos, tablePos + this.raw.length) !== this.raw) {
        tablePos = doc.toString().indexOf(this.raw)
        if (tablePos < 0) return
      }
      const tableEnd = tablePos + this.raw.length
      if (this.colWidthsLine) {
        // 更新現有注釋
        const commentStart = tableEnd + 1  // +1 for newline
        const commentEnd = commentStart + this.colWidthsLine.length
        if (commentEnd <= doc.length) {
          _view.dispatch({ changes: { from: commentStart, to: commentEnd, insert: newComment } })
        }
      } else {
        // 插入新注釋
        const insertPos = tableEnd < doc.length ? tableEnd : doc.length
        const insert = tableEnd < doc.length ? `\n${newComment}` : `\n${newComment}`
        _view.dispatch({ changes: { from: insertPos, to: insertPos, insert } })
      }
    }

    const applyFixedLayout = () => {
      if (table.style.tableLayout === 'fixed') return
      const widths = headerCells.map(hc => hc.offsetWidth)
      table.querySelectorAll('tr').forEach(tr => {
        const cells = tr.querySelectorAll('th, td')
        widths.forEach((w, i) => {
          const cell = cells[i] as HTMLElement | undefined
          if (cell) cell.style.width = w + 'px'
        })
      })
      table.style.tableLayout = 'fixed'
      table.style.width = table.offsetWidth + 'px'
    }

    const applyWidths = (widths: number[]) => {
      if (widths.length !== headerCells.length) return
      _tableColWidths.set(cacheKey, widths)
      // 套用到每一 row 的對應 cell（fixed layout 需要每欄都有明確寬度）
      table.querySelectorAll('tr').forEach(tr => {
        const cells = tr.querySelectorAll('th, td')
        widths.forEach((w, i) => {
          const cell = cells[i] as HTMLElement | undefined
          if (cell) cell.style.width = w + 'px'
        })
      })
      table.style.tableLayout = 'fixed'
      table.style.width = '100%'
    }

    // 優先從 markdown 注釋還原欄寬（持久化來源）
    const fromComment = this.colWidthsLine.match(/<!-- col-widths: ([\d,]+) -->/)
    if (fromComment) {
      applyWidths(fromComment[1].split(',').map(Number))
    } else {
      // fallback：session 內記憶體快取
      const saved = _tableColWidths.get(cacheKey)
      if (saved) applyWidths(saved)
    }

    headerCells.forEach((th, thIndex) => {
      th.style.position = 'relative'

      const handle = document.createElement('div')
      handle.style.cssText = [
        'position:absolute;right:0;top:10%;bottom:10%;width:4px;',
        'cursor:col-resize;z-index:10;',
        'background:var(--color-border);border-radius:2px;',
        'transition:background 0.15s,width 0.15s;',
      ].join('')

      handle.addEventListener('mouseenter', () => {
        handle.style.background = 'var(--color-accent)'
        handle.style.width = '4px'
      })
      handle.addEventListener('mouseleave', () => {
        handle.style.background = 'var(--color-border)'
        handle.style.width = '4px'
      })

      handle.addEventListener('mousedown', (e: MouseEvent) => {
        e.preventDefault()
        e.stopPropagation()
        applyFixedLayout()

        const startX = e.clientX
        const startWidth = th.offsetWidth

        const onMove = (ev: MouseEvent) => {
          const newWidth = Math.max(40, startWidth + ev.clientX - startX)
          th.style.width = newWidth + 'px'
          table.querySelectorAll('tr').forEach(tr => {
            const cell = tr.querySelectorAll('th, td')[thIndex] as HTMLElement | undefined
            if (cell && cell !== th) cell.style.width = newWidth + 'px'
          })
        }
        const onUp = () => {
          document.removeEventListener('mousemove', onMove)
          document.removeEventListener('mouseup', onUp)
          handle.style.background = 'transparent'
          saveWidths()
        }
        document.addEventListener('mousemove', onMove)
        document.addEventListener('mouseup', onUp)
      })

      th.appendChild(handle)
    })
  }

  // ── 右鍵選單 ──────────────────────────────────────────────────────────────
  private showContextMenu(
    x: number, y: number,
    rowIndex: number, colIndex: number,
    _cellSpan?: HTMLElement,
    savedSel?: { text: string; domStart: number; domEnd: number; currentRaw: () => string } | null,
    onApplyFormat?: (newContent: string) => void,
  ) {
    // 移除已有選單
    document.querySelectorAll('.cm-table-ctx-menu').forEach(el => el.remove())

    const lines = this.raw.split('\n')
    const dataRowCount = lines.filter((_, i) => i !== 1).length - 1
    const colCount = parseTableCells(lines[0]).length

    const menu = document.createElement('div')
    menu.className = 'cm-table-ctx-menu'
    menu.style.cssText = [
      `position:fixed;left:${x}px;top:${y}px;`,
      'background:var(--color-bg-elevated);border:1px solid var(--color-border);',
      'border-radius:6px;box-shadow:0 4px 16px rgba(0,0,0,0.35);',
      'z-index:9999;min-width:180px;overflow:hidden;padding:4px 0;',
      'font-size:13px;',
    ].join('')

    // ── helper: 建立選單按鈕 ─────────────────────────────────────────────────
    const makeBtn = (label: string, danger: boolean, action: () => void) => {
      const btn = document.createElement('button')
      btn.textContent = label
      btn.style.cssText = [
        'display:block;width:100%;text-align:left;background:none;border:none;',
        'padding:6px 14px;cursor:pointer;',
        danger ? 'color:var(--color-danger);' : 'color:var(--color-text-primary);',
      ].join('')
      btn.addEventListener('mouseenter', () => { btn.style.background = 'var(--color-bg-hover)' })
      btn.addEventListener('mouseleave', () => { btn.style.background = 'none' })
      btn.addEventListener('mousedown', (e: MouseEvent) => {
        e.preventDefault(); e.stopPropagation()
        menu.remove(); action()
      })
      return btn
    }
    // 純字串操作：segment-based 樣式合併，不產生巢狀
    const applyFormatToSelection = (newProps: StyleProps) => {
      if (!savedSel || !onApplyFormat) return
      const currentRaw = savedSel.currentRaw()
      const newContent = applyStyleToRange(currentRaw, savedSel.domStart, savedSel.domEnd, newProps)
      onApplyFormat(newContent)
    }

    const makeSep = () => {
      const sep = document.createElement('div')
      sep.style.cssText = 'height:1px;background:var(--color-border);margin:4px 0;'
      return sep
    }

    // ── 主選單 ───────────────────────────────────────────────────────────────
    const mainPanel = document.createElement('div')
    mainPanel.style.padding = '4px 0'

    const rowColItems: Array<{ label: string; action: () => void; danger?: boolean }> = [
      { label: '↑ 上方插入一行', action: () => this.insertRow(rowIndex, 'above') },
      { label: '↓ 下方插入一行', action: () => this.insertRow(rowIndex, 'below') },
      { label: '← 左側插入一欄', action: () => this.insertCol(colIndex, 'left') },
      { label: '→ 右側插入一欄', action: () => this.insertCol(colIndex, 'right') },
      { label: '', action: () => {} },
      { label: '刪除此行' + (dataRowCount <= 1 && rowIndex > 0 ? '（最後一行）' : ''), action: () => this.deleteRow(rowIndex), danger: true },
      { label: '刪除此欄' + (colCount <= 1 ? '（最後一欄）' : ''), action: () => this.deleteCol(colIndex), danger: true },
    ]

    rowColItems.forEach(item => {
      if (!item.label) { mainPanel.appendChild(makeSep()); return }
      mainPanel.appendChild(makeBtn(item.label, item.danger ?? false, item.action))
    })

    // 有框選文字時，加入改變顏色 / 改變字型
    if (savedSel) {
      mainPanel.appendChild(makeSep())

      const makeNavBtn = (label: string, showPanel: () => void) => {
        const btn = document.createElement('button')
        btn.style.cssText = [
          'display:flex;width:100%;justify-content:space-between;align-items:center;',
          'background:none;border:none;padding:6px 14px;cursor:pointer;font-size:13px;',
          'color:var(--color-text-primary);',
        ].join('')
        const lbl = document.createElement('span'); lbl.textContent = label
        const arr = document.createElement('span'); arr.textContent = '›'; arr.style.color = 'var(--color-text-muted)'
        btn.appendChild(lbl); btn.appendChild(arr)
        btn.addEventListener('mouseenter', () => { btn.style.background = 'var(--color-bg-hover)' })
        btn.addEventListener('mouseleave', () => { btn.style.background = 'none' })
        btn.addEventListener('mousedown', (e: MouseEvent) => {
          e.preventDefault(); e.stopPropagation()
          mainPanel.style.display = 'none'; showPanel()
        })
        return btn
      }

      // ── 改變顏色 panel ─────────────────────────────────────────────────────
      const colorPanel = document.createElement('div')
      colorPanel.style.cssText = 'display:none;padding:12px;'

      const buildColorPanel = () => {
        colorPanel.innerHTML = ''
        let pickedColor = '#e03030'
        const COLORS = ['#e03030','#e07830','#d4c020','#30a850','#2080e0','#8040e0','#e030a0','#888888','#000000']

        const title = document.createElement('div')
        title.textContent = '改變顏色'
        title.style.cssText = 'font-size:12px;font-weight:600;color:var(--color-text-secondary);margin-bottom:8px;'
        colorPanel.appendChild(title)

        // 先建 customInput 和 previewSpan，讓 swatches closure 能引用
        const customInput = document.createElement('input')
        customInput.type = 'color'; customInput.value = pickedColor
        customInput.style.cssText = 'width:32px;height:24px;padding:0;border:1px solid var(--color-border);border-radius:4px;cursor:pointer;'

        const previewSpan = document.createElement('span')
        previewSpan.textContent = savedSel.text.slice(0, 30) + (savedSel.text.length > 30 ? '…' : '')
        previewSpan.style.color = pickedColor

        const swatches = document.createElement('div')
        swatches.style.cssText = 'display:flex;flex-wrap:wrap;gap:6px;margin-bottom:8px;'
        COLORS.forEach(c => {
          const sw = document.createElement('div')
          sw.style.cssText = `width:22px;height:22px;border-radius:4px;background:${c};cursor:pointer;box-sizing:border-box;border:2px solid ${c === pickedColor ? 'var(--color-text-primary)' : 'transparent'};`
          sw.addEventListener('mousedown', (e) => {
            e.preventDefault(); e.stopPropagation()
            pickedColor = c
            swatches.querySelectorAll('div').forEach((s, i) => {
              (s as HTMLElement).style.border = `2px solid ${COLORS[i] === c ? 'var(--color-text-primary)' : 'transparent'}`
            })
            customInput.value = c
            previewSpan.style.color = c
          })
          swatches.appendChild(sw)
        })
        colorPanel.appendChild(swatches)

        const customRow = document.createElement('label')
        customRow.style.cssText = 'display:flex;align-items:center;gap:8px;font-size:12px;color:var(--color-text-muted);margin-bottom:8px;'
        customRow.textContent = '自訂 '
        customInput.addEventListener('input', () => { pickedColor = customInput.value; previewSpan.style.color = pickedColor })
        customRow.appendChild(customInput)
        colorPanel.appendChild(customRow)

        const preview = document.createElement('div')
        preview.style.cssText = 'padding:6px 10px;border-radius:6px;background:var(--color-bg-base);font-size:13px;border:1px solid var(--color-border);margin-bottom:8px;'
        preview.appendChild(previewSpan)
        colorPanel.appendChild(preview)

        const btnRow = document.createElement('div')
        btnRow.style.cssText = 'display:flex;gap:6px;'
        const applyBtn = document.createElement('button')
        applyBtn.textContent = '套用'
        applyBtn.style.cssText = 'flex:1;padding:5px;border-radius:5px;background:var(--color-accent);color:white;font-size:12px;cursor:pointer;border:none;'
        applyBtn.addEventListener('mousedown', (e) => {
          e.preventDefault(); e.stopPropagation()
          menu.remove()
          applyFormatToSelection({ color: pickedColor })
        })
        const backBtn = document.createElement('button')
        backBtn.textContent = '返回'
        backBtn.style.cssText = 'flex:1;padding:5px;border-radius:5px;background:var(--color-bg-hover);color:var(--color-text-secondary);font-size:12px;cursor:pointer;border:none;'
        backBtn.addEventListener('mousedown', (e) => {
          e.preventDefault(); e.stopPropagation()
          colorPanel.style.display = 'none'; mainPanel.style.display = 'block'
        })
        btnRow.appendChild(applyBtn); btnRow.appendChild(backBtn)
        colorPanel.appendChild(btnRow)
      }
      buildColorPanel()

      // ── 改變字型 panel ─────────────────────────────────────────────────────
      const fontPanel = document.createElement('div')
      fontPanel.style.cssText = 'display:none;padding:12px;'

      const buildFontPanel = () => {
        fontPanel.innerHTML = ''
        const title = document.createElement('div')
        title.textContent = '改變字型'
        title.style.cssText = 'font-size:12px;font-weight:600;color:var(--color-text-secondary);margin-bottom:8px;'
        fontPanel.appendChild(title)

        const makeRow = (labelText: string, input: HTMLElement) => {
          const row = document.createElement('label')
          row.style.cssText = 'display:flex;flex-direction:column;gap:4px;font-size:12px;color:var(--color-text-muted);margin-bottom:8px;'
          row.textContent = labelText + ' '
          row.appendChild(input); return row
        }
        const selectStyle = 'padding:4px 6px;border-radius:4px;border:1px solid var(--color-border);background:var(--color-bg-base);color:var(--color-text-primary);font-size:12px;'

        const familySel = document.createElement('select')
        familySel.style.cssText = selectStyle
        ;['inherit','Arial','Georgia','Courier New','Times New Roman','Noto Serif TC'].forEach(f => {
          const opt = document.createElement('option'); opt.value = f; opt.textContent = f; familySel.appendChild(opt)
        })
        fontPanel.appendChild(makeRow('字型', familySel))

        const sizeInput = document.createElement('input')
        sizeInput.type = 'number'; sizeInput.placeholder = '預設'; sizeInput.min = '8'; sizeInput.max = '72'
        sizeInput.style.cssText = selectStyle + 'width:100%;box-sizing:border-box;'
        fontPanel.appendChild(makeRow('大小（px）', sizeInput))

        const weightSel = document.createElement('select')
        weightSel.style.cssText = selectStyle
        ;['inherit','normal','bold'].forEach(w => {
          const opt = document.createElement('option'); opt.value = w; opt.textContent = w; weightSel.appendChild(opt)
        })
        fontPanel.appendChild(makeRow('粗細', weightSel))

        const btnRow = document.createElement('div')
        btnRow.style.cssText = 'display:flex;gap:6px;margin-top:4px;'
        const applyBtn = document.createElement('button')
        applyBtn.textContent = '套用'
        applyBtn.style.cssText = 'flex:1;padding:5px;border-radius:5px;background:var(--color-accent);color:white;font-size:12px;cursor:pointer;border:none;'
        applyBtn.addEventListener('mousedown', (e) => {
          e.preventDefault(); e.stopPropagation()
          menu.remove()
          const props: StyleProps = {}
          if (familySel.value && familySel.value !== 'inherit') props.fontFamily = familySel.value
          if (sizeInput.value) props.fontSize = sizeInput.value
          if (weightSel.value && weightSel.value !== 'inherit') props.fontWeight = weightSel.value
          applyFormatToSelection(props)
        })
        const backBtn = document.createElement('button')
        backBtn.textContent = '返回'
        backBtn.style.cssText = 'flex:1;padding:5px;border-radius:5px;background:var(--color-bg-hover);color:var(--color-text-secondary);font-size:12px;cursor:pointer;border:none;'
        backBtn.addEventListener('mousedown', (e) => {
          e.preventDefault(); e.stopPropagation()
          fontPanel.style.display = 'none'; mainPanel.style.display = 'block'
        })
        btnRow.appendChild(applyBtn); btnRow.appendChild(backBtn)
        fontPanel.appendChild(btnRow)
      }
      buildFontPanel()

      mainPanel.appendChild(makeNavBtn('改變顏色', () => { colorPanel.style.display = 'block' }))
      mainPanel.appendChild(makeNavBtn('改變字型', () => { fontPanel.style.display = 'block' }))
      menu.appendChild(colorPanel)
      menu.appendChild(fontPanel)
    }

    menu.appendChild(mainPanel)

    document.body.appendChild(menu)

    // 確保選單不超出視窗
    const rect = menu.getBoundingClientRect()
    if (rect.right > window.innerWidth) menu.style.left = (x - rect.width) + 'px'
    if (rect.bottom > window.innerHeight) menu.style.top = (y - rect.height) + 'px'

    const close = (e: MouseEvent) => {
      if (!menu.contains(e.target as Node)) { menu.remove(); document.removeEventListener('mousedown', close) }
    }
    setTimeout(() => document.addEventListener('mousedown', close), 0)
  }

  // ── 修改 markdown 的輔助方法 ──────────────────────────────────────────────
  private resolveTablePos(): number {
    if (!_view) return -1
    const doc = _view.state.doc
    let pos = this.tableFrom
    if (doc.sliceString(pos, pos + this.raw.length) !== this.raw) {
      pos = doc.toString().indexOf(this.raw)
    }
    return pos
  }

  private replaceRaw(newRaw: string, newColWidthsLine?: string) {
    if (!_view) return
    const pos = this.resolveTablePos()
    if (pos < 0) return
    if (newColWidthsLine !== undefined) {
      if (this.colWidthsLine) {
        // 更新現有注釋（一次 dispatch）
        const commentStart = pos + this.raw.length + 1
        const commentEnd   = commentStart + this.colWidthsLine.length
        if (commentEnd <= _view.state.doc.length) {
          _view.dispatch({ changes: [
            { from: pos, to: pos + this.raw.length, insert: newRaw },
            { from: commentStart, to: commentEnd, insert: newColWidthsLine },
          ]})
          return
        }
      } else {
        // 插入新注釋（緊接在 table 後面）
        const tableEnd = pos + this.raw.length
        _view.dispatch({ changes: { from: pos, to: tableEnd, insert: `${newRaw}\n${newColWidthsLine}` } })
        return
      }
    }
    _view.dispatch({ changes: { from: pos, to: pos + this.raw.length, insert: newRaw } })
  }

  private insertRow(rowIndex: number, where: 'above' | 'below') {
    const lines = this.raw.split('\n')
    // rowIndex 0 = header；markdown 中 index 0=header, 1=separator, 2+=data
    const mdIndex = rowIndex === 0 ? 0 : rowIndex + 1
    const colCount = parseTableCells(lines[0]).length
    const emptyRow = buildTableRow(Array(colCount).fill(''))
    const insertAt = where === 'above' ? Math.max(mdIndex, 2) : Math.min(mdIndex + 1, lines.length)
    // 不允許插入在 separator（index 1）之前的 data 行
    const safeInsert = Math.max(insertAt, 2)
    lines.splice(safeInsert, 0, emptyRow)
    this.replaceRaw(lines.join('\n'))
  }

  private deleteRow(rowIndex: number) {
    const lines = this.raw.split('\n')
    if (rowIndex === 0) return // 不刪 header
    const mdIndex = rowIndex + 1
    if (mdIndex >= lines.length) return
    lines.splice(mdIndex, 1)
    this.replaceRaw(lines.join('\n'))
  }

  private insertCol(colIndex: number, where: 'left' | 'right') {
    const lines = this.raw.split('\n')
    const newLines = lines.map((line, i) => {
      const cells = parseTableCells(line)
      if (i === 1) {
        const newCell = cells[colIndex]?.replace(/[^-:]/g, '-') || '---'
        cells.splice(where === 'left' ? colIndex : colIndex + 1, 0, newCell)
      } else {
        cells.splice(where === 'left' ? colIndex : colIndex + 1, 0, '')
      }
      return buildTableRow(cells)
    })

    // 更新欄寬注釋：在對應位置插入預設寬度 100
    const insertAt = where === 'left' ? colIndex : colIndex + 1
    const existingWidths = this.colWidthsLine.match(/<!-- col-widths: ([\d,]+) -->/)
    const baseWidths = existingWidths
      ? existingWidths[1].split(',').map(Number)
      : Array(parseTableCells(this.raw.split('\n')[0]).length).fill(100) as number[]
    baseWidths.splice(insertAt, 0, 100)
    const newColWidthsLine = `<!-- col-widths: ${baseWidths.join(',')} -->`

    this.replaceRaw(newLines.join('\n'), newColWidthsLine)
  }

  private deleteCol(colIndex: number) {
    const lines = this.raw.split('\n')
    const colCount = parseTableCells(lines[0]).length
    if (colCount <= 1) return
    const newLines = lines.map(line => {
      const cells = parseTableCells(line)
      cells.splice(colIndex, 1)
      return buildTableRow(cells)
    })

    // 更新欄寬注釋：移除對應欄
    let newColWidthsLine: string | undefined
    if (this.colWidthsLine) {
      const m = this.colWidthsLine.match(/<!-- col-widths: ([\d,]+) -->/)
      if (m) {
        const widths = m[1].split(',').map(Number)
        widths.splice(colIndex, 1)
        newColWidthsLine = widths.length > 0 ? `<!-- col-widths: ${widths.join(',')} -->` : ''
      }
    }

    this.replaceRaw(newLines.join('\n'), newColWidthsLine)
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
            'paste', 'cut', 'copy',
            'contextmenu'].includes(e.type)
  }

  private deleteBlock() {
    if (!_view) return
    const doc = _view.state.doc
    let pos = this.tableFrom
    if (doc.sliceString(pos, pos + this.raw.length) !== this.raw) {
      pos = doc.toString().indexOf(this.raw)
      if (pos < 0) return
    }
    // 若緊接有 col-widths 注釋，一起刪除
    let endPos = Math.min(pos + this.raw.length + 1, doc.length)
    if (this.colWidthsLine) {
      endPos = Math.min(endPos + this.colWidthsLine.length + 1, doc.length)
    }
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
    if (!(other instanceof TableWidget)) return false
    const o = other as TableWidget
    return o.raw === this.raw && o.colWidthsLine === this.colWidthsLine
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

    if (this.src.startsWith('http') || this.src.startsWith('data:') || this.src.startsWith('asset:')) {
      img.src = this.src
    } else if (this.src.startsWith('vault://localhost/')) {
      // vault:// scheme is not supported on Windows WebView2 — extract relative path and load as base64
      loadLocal(decodeURIComponent(this.src.slice('vault://localhost/'.length)))
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
        let tableBlockTo     = tableLastLine.to < doc.length ? tableLastLine.to + 1 : doc.length
        let raw = doc.sliceString(tableFirstLine.from, tableLastLine.to)

        // 若緊接的下一行是 col-widths 注釋，把它納入 widget 範圍
        let colWidthsLine = ''
        if (tableLastLine.to + 1 < doc.length) {
          const nextLine = doc.lineAt(tableLastLine.to + 1)
          if (/^<!-- col-widths: [\d,]+ -->$/.test(nextLine.text.trim())) {
            colWidthsLine = nextLine.text.trim()
            tableBlockTo = nextLine.to < doc.length ? nextLine.to + 1 : doc.length
          }
        }

        decos.push({
          from: tableFirstLine.from, to: tableBlockTo,
          deco: Decoration.replace({ widget: new TableWidget(raw, tableFirstLine.from, colWidthsLine), block: true }),
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
// NOTE: We intentionally iterate the full document instead of view.visibleRanges.
// On Windows WebView2, visibleRanges can be empty or stale after display:none→block
// transitions (even after requestMeasure + double rAF), which would cause all
// inline decorations to silently disappear. For typical note-length documents
// (<50KB) the performance difference is negligible.
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

  // Use full document range — avoids visibleRanges timing issues on Windows WebView2
  const fullRange = [{ from: 0, to: doc.length }]
  for (const { from: vpFrom, to: vpTo } of fullRange) {
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
  for (const { from: vpFrom, to: vpTo } of fullRange) {
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
  for (const { from: vpFrom, to: vpTo } of fullRange) {
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
  for (const { from: vpFrom, to: vpTo } of fullRange) {
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

  // ── {color:#xxx}text{/color} syntax ─────────────────────────────────────────
  const colorTagRe = /\{color:([^}\n]+)\}(.*?)\{\/color\}/g
  for (const { from: vpFrom, to: vpTo } of fullRange) {
    const text = doc.sliceString(vpFrom, vpTo)
    let m: RegExpExecArray | null
    colorTagRe.lastIndex = 0
    while ((m = colorTagRe.exec(text)) !== null) {
      const mFrom = vpFrom + m.index
      const mTo = mFrom + m[0].length
      const color = m[1].trim()
      const openTag = `{color:${m[1]}}`
      const innerFrom = mFrom + openTag.length
      const innerTo = mTo - '{/color}'.length
      if (innerFrom >= innerTo) continue
      if (!cursorIn(mFrom, mTo)) {
        add(mFrom, innerFrom, Decoration.mark({ class: 'cm-live-hidden' }))
        add(innerFrom, innerTo, Decoration.mark({ attributes: { style: `color:${color}` } }))
        add(innerTo, mTo, Decoration.mark({ class: 'cm-live-hidden' }))
      } else {
        add(mFrom, innerFrom, Decoration.mark({ class: 'cm-live-syntax-mark' }))
        add(innerFrom, innerTo, Decoration.mark({ attributes: { style: `color:${color}` } }))
        add(innerTo, mTo, Decoration.mark({ class: 'cm-live-syntax-mark' }))
      }
    }
  }

  // ── {font:family;size;weight}text{/font} syntax ───────────────────────────
  const fontTagRe = /\{font:([^}\n]*)\}(.*?)\{\/font\}/g
  for (const { from: vpFrom, to: vpTo } of fullRange) {
    const text = doc.sliceString(vpFrom, vpTo)
    let m: RegExpExecArray | null
    fontTagRe.lastIndex = 0
    while ((m = fontTagRe.exec(text)) !== null) {
      const mFrom = vpFrom + m.index
      const mTo = mFrom + m[0].length
      const fontSpec = m[1]
      const parts = fontSpec.split(';')
      const family = (parts[0] || '').trim()
      const size = (parts[1] || '').trim()
      const weight = (parts[2] || '').trim()
      const styles: string[] = []
      if (family && family !== 'inherit') {
        styles.push(`font-family:${family.includes(' ') ? `'${family}'` : family}`)
      }
      if (size) styles.push(`font-size:${size}px`)
      if (weight && weight !== 'inherit') styles.push(`font-weight:${weight}`)
      if (!styles.length) continue
      const openTag = `{font:${fontSpec}}`
      const innerFrom = mFrom + openTag.length
      const innerTo = mTo - '{/font}'.length
      if (innerFrom >= innerTo) continue
      if (!cursorIn(mFrom, mTo)) {
        add(mFrom, innerFrom, Decoration.mark({ class: 'cm-live-hidden' }))
        add(innerFrom, innerTo, Decoration.mark({ attributes: { style: styles.join(';') } }))
        add(innerTo, mTo, Decoration.mark({ class: 'cm-live-hidden' }))
      } else {
        add(mFrom, innerFrom, Decoration.mark({ class: 'cm-live-syntax-mark' }))
        add(innerFrom, innerTo, Decoration.mark({ attributes: { style: styles.join(';') } }))
        add(innerTo, mTo, Decoration.mark({ class: 'cm-live-syntax-mark' }))
      }
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
