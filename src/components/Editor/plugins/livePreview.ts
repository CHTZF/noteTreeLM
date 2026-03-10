import { ViewPlugin, ViewUpdate, Decoration, DecorationSet, WidgetType, EditorView } from '@codemirror/view'
import { syntaxTree } from '@codemirror/language'
import { RangeSetBuilder, Text, StateField, Transaction, EditorState } from '@codemirror/state'
import type { Extension } from '@codemirror/state'
import type { SyntaxNodeRef } from '@lezer/common'
import MarkdownIt from 'markdown-it'
import mk from 'markdown-it-texmath'
import katex from 'katex'
import DOMPurify from 'dompurify'

// ── Shared markdown renderer ──────────────────────────────────────────────────
const mdBlock = new MarkdownIt({ html: true, linkify: true, typographer: true, breaks: true })
  .use(mk, { engine: katex, delimiters: 'dollars', katexOptions: { throwOnError: false } })

// ── Block Widget ──────────────────────────────────────────────────────────────
class BlockWidget extends WidgetType {
  constructor(private raw: string) { super() }

  toDOM(): HTMLElement {
    const div = document.createElement('div')
    div.className = 'preview-content cm-live-block-widget'
    div.style.cssText = 'padding:0;margin:0;cursor:text;'
    try {
      const html = DOMPurify.sanitize(
        mdBlock.render(this.raw),
        { ADD_ATTR: ['class', 'style', 'data-copy'] }
      )
      div.innerHTML = html || this.raw
    } catch {
      div.style.cssText += ';white-space:pre-wrap;'
      div.textContent = this.raw
    }
    return div
  }

  ignoreEvent(): boolean { return false }
  eq(other: WidgetType): boolean {
    return other instanceof BlockWidget && (other as BlockWidget).raw === this.raw
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
  eq(_: WidgetType): boolean { return true }
}
const hrWidget = new HRWidget()

type DecoEntry = { from: number; to: number; deco: Decoration }

// ── Helper: compute line-boundary-aligned block range ─────────────────────────
function addBlockWidget(decos: DecoEntry[], doc: Text, nodeFrom: number, nodeTo: number, raw: string) {
  if (nodeFrom >= nodeTo) return
  const fromLine = doc.lineAt(nodeFrom)
  const lastPos  = (nodeTo > nodeFrom && doc.lineAt(nodeTo).from === nodeTo) ? nodeTo - 1 : nodeTo
  const toLine   = doc.lineAt(lastPos)
  const blockFrom = fromLine.from
  const blockTo   = toLine.to < doc.length ? toLine.to + 1 : doc.length
  if (blockFrom >= blockTo) return
  decos.push({ from: blockFrom, to: blockTo, deco: Decoration.replace({ widget: new BlockWidget(raw), block: true }) })
}

// ── Block decoration builder (entire document — required for block:true) ──────
function buildBlockDecos(state: EditorState): DecorationSet {
  const cursor = state.selection.main.head
  const doc    = state.doc
  const decos: DecoEntry[] = []
  const cursorIn = (from: number, to: number) => cursor >= from && cursor <= to

  syntaxTree(state).iterate({
    enter(node: SyntaxNodeRef) {
      const { from, to, name } = node

      if (name === 'Table') {
        if (!cursorIn(from, to)) {
          const effTo = to > from && doc.lineAt(to).from === to ? to - 1 : to
          const raw = doc.sliceString(doc.lineAt(from).from, doc.lineAt(effTo).to)
          addBlockWidget(decos, doc, from, to, raw)
        }
        return false
      }

      if (name === 'FencedCode') {
        if (!cursorIn(from, to)) {
          const effTo = to > from && doc.lineAt(to).from === to ? to - 1 : to
          const raw = doc.sliceString(doc.lineAt(from).from, doc.lineAt(effTo).to)
          addBlockWidget(decos, doc, from, to, raw)
        }
        return false
      }

      if (name === 'HorizontalRule') {
        if (!cursorIn(from, to)) {
          const hrFromLine = doc.lineAt(from)
          const hrLastPos  = (to > from && doc.lineAt(to).from === to) ? to - 1 : to
          const hrToLine   = doc.lineAt(hrLastPos)
          const hrFrom = hrFromLine.from
          const hrTo   = hrToLine.to < doc.length ? hrToLine.to + 1 : doc.length
          if (hrFrom < hrTo) {
            decos.push({ from: hrFrom, to: hrTo, deco: Decoration.replace({ widget: hrWidget, block: true }) })
          }
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

// ── StateField for block-level decorations ────────────────────────────────────
// CM6 rule: block:true decorations MUST come from EditorView.decorations (StateField),
// NOT from ViewPlugin, or CM6 throws "Block decorations may not be specified via plugins"
const liveBlockField = StateField.define<DecorationSet>({
  create(state) {
    try { return buildBlockDecos(state) } catch { return Decoration.none }
  },
  update(decos, tr: Transaction) {
    if (tr.docChanged || tr.selection) {
      try { return buildBlockDecos(tr.state) } catch { return Decoration.none }
    }
    return decos.map(tr.changes)
  },
  provide: f => EditorView.decorations.from(f),
})

// ── Inline decoration builder (visible ranges only) ───────────────────────────
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

        // Skip block elements — handled by StateField
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

// ── ViewPlugin for inline decorations ────────────────────────────────────────
const liveInlinePlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet
    constructor(view: EditorView) {
      try { this.decorations = buildInlineDecos(view) } catch { this.decorations = Decoration.none }
    }
    update(update: ViewUpdate) {
      if (update.docChanged || update.selectionSet || update.viewportChanged) {
        try { this.decorations = buildInlineDecos(update.view) } catch { this.decorations = Decoration.none }
      }
    }
  },
  { decorations: (v) => v.decorations },
)

// ── Export: combined extension (StateField + ViewPlugin) ──────────────────────
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
  '.cm-live-block-widget': { display: 'block', width: '100%', boxSizing: 'border-box' },
})
