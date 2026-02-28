import { ViewPlugin, Decoration, DecorationSet, ViewUpdate } from '@codemirror/view'
import { RangeSetBuilder } from '@codemirror/state'

const WIKILINK_RE = /!?\[\[([^\[\]]+)\]\]/g

export const wikilinkPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet

    constructor(view: import('@codemirror/view').EditorView) {
      this.decorations = this.buildDecorations(view)
    }

    update(update: ViewUpdate) {
      if (update.docChanged || update.viewportChanged) {
        this.decorations = this.buildDecorations(update.view)
      }
    }

    buildDecorations(view: import('@codemirror/view').EditorView): DecorationSet {
      const builder = new RangeSetBuilder<Decoration>()
      const doc = view.state.doc.toString()
      let match: RegExpExecArray | null

      WIKILINK_RE.lastIndex = 0
      while ((match = WIKILINK_RE.exec(doc)) !== null) {
        const from = match.index
        const to = from + match[0].length
        builder.add(
          from,
          to,
          Decoration.mark({ class: 'cm-wikilink-mark' })
        )
      }

      return builder.finish()
    }
  },
  { decorations: (v) => v.decorations }
)
