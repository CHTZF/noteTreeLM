import { useEffect, useState } from 'react'
import { getCurrentWindow } from '@tauri-apps/api/window'

interface TitleBarProps {
  title?: string
}

const isWindows = navigator.userAgent.includes('Windows')

export default function TitleBar({ title }: TitleBarProps) {
  const [isMaximized, setIsMaximized] = useState(false)

  useEffect(() => {
    if (!isWindows) return
    const win = getCurrentWindow()
    win.isMaximized().then(setIsMaximized)
    const unlisten = win.onResized(() => {
      win.isMaximized().then(setIsMaximized)
    })
    return () => { unlisten.then(fn => fn()) }
  }, [])

  if (!isWindows) {
    return (
      <div data-tauri-drag-region className="app-titlebar">
        <span className="app-titlebar-title" data-tauri-drag-region>
          {title || 'noteTreeLM'}
        </span>
      </div>
    )
  }

  const win = getCurrentWindow()

  return (
    <div data-tauri-drag-region className="app-titlebar">
      <span className="app-titlebar-title" data-tauri-drag-region>
        {title || 'noteTreeLM'}
      </span>
      {/* Windows 視窗控制按鈕 */}
      <div className="app-titlebar-controls" onMouseDown={e => e.stopPropagation()}>
        <button
          className="titlebar-btn titlebar-btn-min"
          title="最小化"
          onClick={() => win.minimize()}
        >
          <svg width="10" height="1" viewBox="0 0 10 1"><rect width="10" height="1" fill="currentColor"/></svg>
        </button>
        <button
          className="titlebar-btn titlebar-btn-max"
          title={isMaximized ? '還原' : '最大化'}
          onClick={() => win.toggleMaximize()}
        >
          {isMaximized
            ? <svg width="10" height="10" viewBox="0 0 10 10"><path d="M3 0H10V7H7V10H0V3H3V0ZM7 3H3V7H7V3Z" fill="currentColor" fillRule="evenodd"/></svg>
            : <svg width="10" height="10" viewBox="0 0 10 10"><rect x="0" y="0" width="10" height="10" fill="none" stroke="currentColor" strokeWidth="1"/></svg>
          }
        </button>
        <button
          className="titlebar-btn titlebar-btn-close"
          title="關閉"
          onClick={() => win.close()}
        >
          <svg width="10" height="10" viewBox="0 0 10 10"><path d="M1 1L9 9M9 1L1 9" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round"/></svg>
        </button>
      </div>
    </div>
  )
}
