import { useEditorStore } from '../../stores/editorStore'
import type { Tab } from '../../stores/tabStore'

export const SPECIAL_NAMES: Record<string, string> = {
  '__graph__': '圖譜',
  '__agent_tools__': '工具測試台',
  '__chat__': 'Chat',
  '__live_chat__': 'Live Chat',
  '__settings__': '個人化設定',
  '__system_settings__': '系統設定',
  '__help__': '說明',
  '__trash__': '垃圾桶',
}

interface TabBarProps {
  tabs: Tab[]
  activeTabId: string | null
  onActivate: (id: string) => void
  onClose: (id: string) => void
  /** Called on mousedown on a tab — parent drives the drag logic */
  onTabMouseDown?: (tabId: string, startX: number, startY: number) => void
  /** Tab id that should show the drag-over insertion indicator */
  dragOverTabId?: string | null
  /** Pane id for drop target detection */
  paneId?: string
  leftContent?: React.ReactNode
  rightContent?: React.ReactNode
}

export default function TabBar({
  tabs, activeTabId, onActivate, onClose,
  onTabMouseDown, dragOverTabId, paneId,
  leftContent, rightContent,
}: TabBarProps) {
  const { isDirty, currentPath } = useEditorStore()

  return (
    <div className="tab-bar" data-pane-id={paneId}>
      {leftContent && <div className="tab-bar-icons">{leftContent}</div>}
      <div className="tab-bar-tabs">
        {tabs.map((tab) => {
          const isActive = tab.id === activeTabId
          const isSpecial = tab.path in SPECIAL_NAMES
          const filename = SPECIAL_NAMES[tab.path] ?? (tab.path.split('/').pop() ?? tab.path)
          const showDirty = !isSpecial && isActive && isDirty && tab.path === currentPath

          return (
            <div
              key={tab.id}
              data-tab-id={tab.id}
              className={`tab-item${isActive ? ' active' : ''}${dragOverTabId === tab.id ? ' drag-over' : ''}`}
              onMouseDown={(e) => {
                if (e.button !== 0) return
                onTabMouseDown?.(tab.id, e.clientX, e.clientY)
              }}
              onClick={() => onActivate(tab.id)}
              title={tab.path}
            >
              {showDirty && <span className="tab-dirty">·</span>}
              <span className="tab-name">{filename}</span>
              <button
                className="tab-close"
                onMouseDown={(e) => e.stopPropagation()}
                onClick={(e) => { e.stopPropagation(); onClose(tab.id) }}
                title="關閉"
              >
                ×
              </button>
            </div>
          )
        })}
      </div>
      {rightContent && <div className="tab-bar-right">{rightContent}</div>}
    </div>
  )
}
