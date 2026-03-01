import { create } from 'zustand'

interface ToastItem {
  id: number
  message: string
  type: 'success' | 'error' | 'warning' | 'info'
}

interface ToastStore {
  toasts: ToastItem[]
  show: (message: string, type?: ToastItem['type'], duration?: number) => number
  dismiss: (id: number) => void
}

let toastId = 0

export const useToastStore = create<ToastStore>((set) => ({
  toasts: [],
  show: (message, type = 'info', duration = 3000) => {
    const id = ++toastId
    set((s) => ({ toasts: [...s.toasts, { id, message, type }] }))
    if (duration > 0) {
      setTimeout(() => {
        set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }))
      }, duration)
    }
    return id
  },
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}))

export const toast = {
  success: (msg: string) => useToastStore.getState().show(msg, 'success'),
  error: (msg: string, opts?: { duration?: number }) =>
    useToastStore.getState().show(msg, 'error', opts?.duration ?? 3000),
  warning: (msg: string) => useToastStore.getState().show(msg, 'warning'),
  info: (msg: string, opts?: { duration?: number }) =>
    useToastStore.getState().show(msg, 'info', opts?.duration ?? 3000),
  dismiss: (id: number) => useToastStore.getState().dismiss(id),
}

export default function Toast() {
  const { toasts, dismiss } = useToastStore()
  return (
    <div id="toast-container">
      {toasts.map((t) => (
        <div
          key={t.id}
          className={`toast toast-${t.type}`}
          onClick={() => dismiss(t.id)}
          style={{
            padding: '8px 16px',
            borderRadius: '6px',
            fontSize: '13px',
            cursor: 'pointer',
            pointerEvents: 'all',
            background: t.type === 'success' ? 'rgba(78,201,176,0.15)' :
                        t.type === 'error' ? 'rgba(224,108,117,0.15)' :
                        t.type === 'warning' ? 'rgba(209,154,102,0.15)' :
                        'rgba(124,140,248,0.15)',
            border: `1px solid ${
              t.type === 'success' ? '#4ec9b0' :
              t.type === 'error' ? '#e06c75' :
              t.type === 'warning' ? '#d19a66' :
              '#7c8cf8'
            }`,
            color: '#c9cdd4',
            minWidth: '200px',
            maxWidth: '360px',
          }}
        >
          {t.message}
        </div>
      ))}
    </div>
  )
}
