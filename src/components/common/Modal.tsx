import { useEffect, ReactNode } from 'react'

interface ModalProps {
  title: string
  onClose: () => void
  children: ReactNode
  width?: number
}

export default function Modal({ title, onClose, children, width = 480 }: ModalProps) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [onClose])

  return (
    <div
      style={{
        position: 'fixed', inset: 0, zIndex: 1000,
        background: 'rgba(0,0,0,0.6)',
        display: 'flex', alignItems: 'center', justifyContent: 'center',
      }}
      onClick={(e) => { if (e.target === e.currentTarget) onClose() }}
    >
      <div style={{
        width, maxWidth: '90vw', maxHeight: '80vh',
        background: '#25262b', borderRadius: '10px',
        border: '1px solid #373a40',
        boxShadow: '0 8px 24px rgba(0,0,0,0.5)',
        display: 'flex', flexDirection: 'column',
      }}>
        <div style={{
          display: 'flex', alignItems: 'center', justifyContent: 'space-between',
          padding: '16px 20px', borderBottom: '1px solid #373a40',
        }}>
          <h3 style={{ fontSize: '15px', fontWeight: 600, color: '#c9cdd4' }}>{title}</h3>
          <button
            onClick={onClose}
            style={{ color: '#8b8fa8', fontSize: '18px', lineHeight: 1 }}
          >✕</button>
        </div>
        <div style={{ padding: '20px', overflowY: 'auto' }}>{children}</div>
      </div>
    </div>
  )
}
