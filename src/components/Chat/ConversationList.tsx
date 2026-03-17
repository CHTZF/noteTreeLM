import { invoke } from '@tauri-apps/api/core'
import { useEffect, useRef, useState } from 'react'
import { FontAwesomeIcon } from '@fortawesome/react-fontawesome'
import { faPlus, faTrash, faClock } from '@fortawesome/free-solid-svg-icons'
import { toast } from '../common/Toast'
import { useAuthStore } from '../../stores/authStore'

export interface ConversationSummary {
  id: string
  mode: string
  title: string
  updated_at: number
  has_pending_plan: boolean
}

interface Props {
  mode: 'chat' | 'live_chat' | 'kb_assist'
  selectedId: string | null
  onSelect: (id: string) => void
  onNew: (id: string) => void
}

const PAGE_SIZE = 50

function groupByDate(convs: ConversationSummary[]): { label: string; items: ConversationSummary[] }[] {
  const today = new Date(); today.setHours(0, 0, 0, 0)
  const yesterday = new Date(today); yesterday.setDate(yesterday.getDate() - 1)
  const todayTs = today.getTime() / 1000
  const yesterdayTs = yesterday.getTime() / 1000

  const groups: Record<string, ConversationSummary[]> = {}
  for (const c of convs) {
    let label: string
    if (c.updated_at >= todayTs) label = '今天'
    else if (c.updated_at >= yesterdayTs) label = '昨天'
    else label = '更早'
    if (!groups[label]) groups[label] = []
    groups[label].push(c)
  }

  return ['今天', '昨天', '更早']
    .filter(l => groups[l])
    .map(l => ({ label: l, items: groups[l] }))
}

export default function ConversationList({ mode, selectedId, onSelect, onNew }: Props) {
  const [convs, setConvs] = useState<ConversationSummary[]>([])
  const [hasMore, setHasMore] = useState(false)
  const [loading, setLoading] = useState(true)
  const loadingRef = useRef(false)

  const loadConvs = async (reset = false) => {
    if (loadingRef.current) return
    loadingRef.current = true
    setLoading(true)
    try {
      const newOffset = reset ? 0 : convs.length
      const username = useAuthStore.getState().session?.username ?? ''
      const list: ConversationSummary[] = await invoke('list_conversations', {
        username,
        mode,
        limit: PAGE_SIZE,
        offset: newOffset,
      })
      setConvs(prev => reset ? list : [...prev, ...list])
      setHasMore(list.length === PAGE_SIZE)
      console.log(`[ConversationList][${mode}] loaded ${list.length} conversations:`, list.map(c => ({ id: c.id, title: c.title, updated_at: new Date(c.updated_at * 1000).toLocaleString() })))
    } catch (e) {
      console.error(`[ConversationList][${mode}] load_conversations error:`, e)
    } finally {
      setLoading(false)
      loadingRef.current = false
    }
  }

  // Always reload from DB on mount — no loadedRef guard
  useEffect(() => {
    loadConvs(true)
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode])

  const handleNew = async () => {
    try {
      const username = useAuthStore.getState().session?.username ?? ''
      const id: string = await invoke('create_conversation', { username, mode })
      setConvs(prev => [{ id, mode, title: '新對話', updated_at: Date.now() / 1000, has_pending_plan: false }, ...prev])
      onNew(id)
    } catch {
      toast.error('建立對話失敗')
    }
  }

  const handleDelete = async (e: React.MouseEvent, id: string) => {
    e.stopPropagation()
    try {
      await invoke('delete_conversation', { id })
      setConvs(prev => prev.filter(c => c.id !== id))
      if (selectedId === id) onNew('')
    } catch {
      toast.error('刪除對話失敗')
    }
  }

  const groups = groupByDate(convs)

  return (
    <div className="conv-list">
      <div className="conv-list-header">
        <button className="conv-new-btn" onClick={handleNew} title="新對話">
          <FontAwesomeIcon icon={faPlus} />
          <span>新對話</span>
        </button>
      </div>

      <div className="conv-list-body">
        {loading && convs.length === 0 && (
          <div className="conv-empty" style={{ opacity: 0.5 }}>載入中…</div>
        )}
        {!loading && groups.length === 0 && (
          <div className="conv-empty">尚無對話記錄</div>
        )}

        {groups.map(group => (
          <div key={group.label} className="conv-group">
            <div className="conv-group-label">{group.label}</div>
            {group.items.map(c => (
              <div
                key={c.id}
                className={`conv-item ${c.id === selectedId ? 'active' : ''}`}
                onClick={() => onSelect(c.id)}
              >
                <div className="conv-item-title">
                  {c.has_pending_plan && (
                    <FontAwesomeIcon icon={faClock} className="conv-pending-badge" title="有計畫等待確認" />
                  )}
                  <span>{c.title || '未命名對話'}</span>
                </div>
                <button
                  className="conv-delete-btn"
                  onClick={e => handleDelete(e, c.id)}
                  title="刪除對話"
                >
                  <FontAwesomeIcon icon={faTrash} />
                </button>
              </div>
            ))}
          </div>
        ))}

        {hasMore && (
          <button className="conv-load-more" onClick={() => loadConvs(false)} disabled={loading}>
            載入更多
          </button>
        )}
      </div>
    </div>
  )
}
