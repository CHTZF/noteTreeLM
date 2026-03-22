import { useEffect } from 'react'
import { useActivityStore } from '../stores/activityStore'

/**
 * 全域 DOM 監聽：selectionchange + copy
 * 只記錄發生在 .editor-content 內的選取，避免 UI 按鈕點選等噪音。
 * 在 App 層 mount 一次即可（AppMain 內 useEffect）。
 */
export function useActivityContext() {
  const addAction = useActivityStore(s => s.addAction)

  useEffect(() => {
    let selTimer: ReturnType<typeof setTimeout>

    const onSelectionChange = () => {
      clearTimeout(selTimer)
      selTimer = setTimeout(() => {
        const sel = window.getSelection()
        const text = sel?.toString().trim()
        if (!text || text.length < 10) return

        // 只記錄 editor 內的選取
        const anchor = sel?.anchorNode
        if (!anchor?.parentElement?.closest('.editor-content, .cm-content, .ProseMirror')) return

        addAction({ type: 'selection', text: text.slice(0, 500) })
      }, 500)
    }

    const onCopy = () => {
      const text = window.getSelection()?.toString().trim()
      if (text && text.length >= 3) {
        addAction({ type: 'copy', text: text.slice(0, 500) })
      }
    }

    document.addEventListener('selectionchange', onSelectionChange)
    document.addEventListener('copy', onCopy)

    return () => {
      clearTimeout(selTimer)
      document.removeEventListener('selectionchange', onSelectionChange)
      document.removeEventListener('copy', onCopy)
    }
  }, [addAction])
}
