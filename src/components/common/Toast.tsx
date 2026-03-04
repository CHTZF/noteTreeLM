import { create } from 'zustand'

interface ToastItem {
  id: number
  message: string
  type: 'success' | 'error' | 'warning' | 'info'
}

interface ToastStore {
  toasts: ToastItem[]
  leavingIds: Set<number>
  show: (message: string, type?: ToastItem['type'], duration?: number) => number
  startDismiss: (id: number) => void
  remove: (id: number) => void
}

let toastId = 0
const ANIM_MS = 300

export const useToastStore = create<ToastStore>((set, get) => ({
  toasts: [],
  leavingIds: new Set<number>(),
  show: (message, type = 'info', duration = 3000) => {
    const id = ++toastId
    set((s) => ({ toasts: [...s.toasts, { id, message, type }] }))
    if (duration > 0) {
      setTimeout(() => get().startDismiss(id), duration)
    }
    return id
  },
  startDismiss: (id) => {
    set((s) => ({ leavingIds: new Set(s.leavingIds).add(id) }))
    setTimeout(() => get().remove(id), ANIM_MS)
  },
  remove: (id) =>
    set((s) => {
      const leavingIds = new Set(s.leavingIds)
      leavingIds.delete(id)
      return { toasts: s.toasts.filter((t) => t.id !== id), leavingIds }
    }),
}))

export const toast = {
  success: (msg: string) => useToastStore.getState().show(msg, 'success'),
  error: (msg: string, opts?: { duration?: number }) =>
    useToastStore.getState().show(msg, 'error', opts?.duration ?? 3000),
  warning: (msg: string) => useToastStore.getState().show(msg, 'warning'),
  info: (msg: string, opts?: { duration?: number }) =>
    useToastStore.getState().show(msg, 'info', opts?.duration ?? 3000),
  dismiss: (id: number) => useToastStore.getState().startDismiss(id),
}

const ICON: Record<ToastItem['type'], string> = {
  success: '✓',
  error:   '✕',
  warning: '⚠',
  info:    'ℹ',
}

const ICON_COLOR: Record<ToastItem['type'], string> = {
  success: '#30d158',
  error:   '#ff453a',
  warning: '#ff9f0a',
  info:    '#0a84ff',
}

export default function Toast() {
  const { toasts, leavingIds, startDismiss } = useToastStore()

  return (
    <div
      style={{
        position: 'fixed',
        top: 16,
        left: '50%',
        transform: 'translateX(-50%)',
        zIndex: 9999,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        gap: 8,
        pointerEvents: 'none',
      }}
    >
      {toasts.map((t) => {
        const isLeaving = leavingIds.has(t.id)
        return (
          <div
            key={t.id}
            onClick={() => startDismiss(t.id)}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 8,
              padding: '10px 18px',
              borderRadius: 24,
              background: 'rgba(28, 28, 30, 0.90)',
              backdropFilter: 'blur(20px)',
              WebkitBackdropFilter: 'blur(20px)',
              boxShadow: '0 4px 24px rgba(0, 0, 0, 0.40), 0 1px 6px rgba(0, 0, 0, 0.25)',
              fontSize: 13,
              fontWeight: 500,
              color: '#ffffff',
              cursor: 'pointer',
              pointerEvents: 'all',
              maxWidth: 340,
              minWidth: 140,
              userSelect: 'none',
              animation: isLeaving
                ? `toast-slide-out ${ANIM_MS}ms cubic-bezier(0.4, 0, 1, 1) forwards`
                : `toast-slide-in ${ANIM_MS}ms cubic-bezier(0, 0, 0.2, 1) forwards`,
            }}
          >
            <span
              style={{
                fontSize: 14,
                fontWeight: 700,
                color: ICON_COLOR[t.type],
                lineHeight: 1,
                flexShrink: 0,
              }}
            >
              {ICON[t.type]}
            </span>
            <span style={{ lineHeight: 1.45 }}>{t.message}</span>
          </div>
        )
      })}
    </div>
  )
}
