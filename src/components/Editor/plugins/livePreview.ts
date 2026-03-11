import { ViewPlugin, ViewUpdate, Decoration, DecorationSet, WidgetType, EditorView } from '@codemirror/view'
import { syntaxTree } from '@codemirror/language'
import { RangeSetBuilder, StateField, Transaction, EditorState } from '@codemirror/state'
import type { Extension } from '@codemirror/state'
import type { SyntaxNodeRef } from '@lezer/common'
import MarkdownIt from 'markdown-it'
import mk from 'markdown-it-texmath'
import katex from 'katex'
import DOMPurify from 'dompurify'

// ── Shared markdown renderer ──────────────────────────────────────────────────
const mdBlock = new MarkdownIt({ html: true, linkify: true, typographer: true, breaks: true })
  .use(mk, { engine: katex, delimiters: 'dollars', katexOptions: { throwOnError: false } })

// ── Module-level EditorView ref — kept up-to-date by liveInlinePlugin ─────────
// Used by TableWidget to dispatch markdown changes on cell blur.
// Safe because there is only one editor view in this app at any time.
let _view: EditorView | null = null

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
      if (table) this.attachEditing(table)
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
    header.appendChild(badge)
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

  decos.sort((a, b) => a.from !== b.from ? a.from - b.from : b.to - a.to)
  const builder = new RangeSetBuilder<Decoration>()
  for (const { from, to, deco } of decos) {
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

// ── Theme ─────────────────────────────────────────────────────────────────────
export const livePreviewTheme = EditorView.theme({
  '.cm-live-hidden': { fontSize: '0 !important', width: '0', display: 'inline-block', overflow: 'hidden' },
  '.cm-live-h1': { fontSize: '1.9em', fontWeight: '700', fontFamily: 'var(--font-sans)', color: 'var(--color-text-primary)', letterSpacing: '-0.02em' },
  '.cm-live-h2': { fontSize: '1.5em',  fontWeight: '700', fontFamily: 'var(--font-sans)', color: 'var(--color-text-primary)' },
  '.cm-live-h3': { fontSize: '1.25em', fontWeight: '600', fontFamily: 'var(--font-sans)', color: 'var(--color-text-primary)' },
  '.cm-live-h4': { fontSize: '1.1em',  fontWeight: '600', fontFamily: 'var(--font-sans)', color: 'var(--color-text-primary)' },
  '.cm-live-h5': { fontSize: '1em',    fontWeight: '600', fontFamily: 'var(--font-sans)', color: 'var(--color-text-secondary)' },
  '.cm-live-h6': { fontSize: '0.95em', fontWeight: '600', fontFamily: 'var(--font-sans)', color: 'var(--color-text-muted)' },
  '.cm-live-bold':   { fontWeight: '700' },
  '.cm-live-italic': { fontStyle: 'italic' },
  '.cm-live-strike': { textDecoration: 'line-through', opacity: '0.7' },
  '.cm-live-code': {
    fontFamily: 'var(--font-mono)', fontSize: '0.88em',
    background: 'var(--color-bg-hover)', borderRadius: '3px',
    padding: '1px 4px', color: 'var(--color-accent)',
  },
  '.cm-live-link': { color: 'var(--color-accent)', textDecoration: 'underline', cursor: 'text' },
  // Block widget container — overflow:hidden creates a BFC so that child margins
  // (e.g. .preview-content table/hr/p have margin:1em 0) do NOT collapse outside
  // the widget div. Without this, those margins are excluded from offsetHeight,
  // causing CM6 to underestimate widget height and mismap click positions.
  '.cm-live-block-widget': {
    display: 'block', width: '100%', boxSizing: 'border-box', overflow: 'hidden',
  },
  // Reset margins for all block-level elements rendered inside live-preview widgets.
  // These elements normally get margin from .preview-content rules (App.css),
  // which would escape the widget container and throw off CM6's height map.
  '.cm-live-block-widget table': { margin: '0' },
  '.cm-live-block-widget hr':    { margin: '0' },
  '.cm-live-block-widget pre':   { margin: '0' },
  '.cm-live-block-widget p':     { margin: '0' },
  '.cm-live-block-widget blockquote': { margin: '0' },
})
