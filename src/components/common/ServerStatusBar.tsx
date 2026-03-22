import { useEffect, useState, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { useTranslation } from 'react-i18next'

type ServerStatus = 'running' | 'loading' | 'stopped' | 'unknown'

interface StatusState {
  llm: ServerStatus
  embedding: ServerStatus
  whisper: ServerStatus
}

const STATUS_COLOR: Record<ServerStatus, string> = {
  running: 'var(--color-success, #4caf50)',
  loading: 'var(--color-warning, #ff9800)',
  stopped: 'var(--color-error, #f44336)',
  unknown: 'var(--color-text-muted, #888)',
}

function Dot({ status }: { status: ServerStatus }) {
  const isAnimated = status === 'running' || status === 'loading'
  return (
    <span
      style={{
        display: 'inline-block',
        width: 7,
        height: 7,
        borderRadius: '50%',
        background: STATUS_COLOR[status],
        flexShrink: 0,
        animation: isAnimated
          ? `server-dot-pulse 2s ease-in-out ${status === 'loading' ? '0.5s' : '0s'} infinite`
          : undefined,
      }}
    />
  )
}

interface ServerItemProps {
  label: string
  status: ServerStatus
  statusLabel: string
}

function ServerItem({ label, status, statusLabel }: ServerItemProps) {
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 5, minWidth: 0 }}>
      <Dot status={status} />
      <span style={{ fontSize: 11, color: 'var(--color-text-secondary)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>
        {label}
      </span>
      <span style={{ fontSize: 10, color: STATUS_COLOR[status], whiteSpace: 'nowrap' }}>
        {statusLabel}
      </span>
    </div>
  )
}

interface Props {
  onOpenSettings?: () => void
}

export default function ServerStatusBar({ onOpenSettings }: Props) {
  const { t } = useTranslation()
  const [status, setStatus] = useState<StatusState>({
    llm: 'unknown',
    embedding: 'unknown',
    whisper: 'unknown',
  })

  const fetchStatus = useCallback(async () => {
    const [llm, embedding, whisper] = await Promise.all([
      invoke<string>('get_llama_server_status').catch(() => 'unknown'),
      invoke<string>('get_embedding_server_status').catch(() => 'unknown'),
      invoke<string>('get_whisper_server_status').catch(() => 'unknown'),
    ])
    setStatus({
      llm: (llm as ServerStatus) || 'unknown',
      embedding: (embedding as ServerStatus) || 'unknown',
      whisper: (whisper as ServerStatus) || 'unknown',
    })
  }, [])

  useEffect(() => {
    fetchStatus()
    const id = setInterval(fetchStatus, 10_000)
    return () => clearInterval(id)
  }, [fetchStatus])

  const statusLabel = (s: ServerStatus) => t(`server_status.${s}`)

  return (
    <>
      <style>{`
        @keyframes server-dot-pulse {
          0%, 100% { opacity: 1; transform: scale(1); }
          50% { opacity: 0.55; transform: scale(0.85); }
        }
      `}</style>
      <div
        title={t('server_status.click_to_open_settings')}
        onClick={onOpenSettings}
        style={{
          borderTop: '1px solid var(--color-border)',
          padding: '7px 12px',
          display: 'flex',
          flexDirection: 'column',
          gap: 5,
          cursor: onOpenSettings ? 'pointer' : 'default',
          background: 'var(--color-bg-secondary, var(--color-sidebar-bg))',
          flexShrink: 0,
          userSelect: 'none',
        }}
      >
        <div style={{ fontSize: 10, color: 'var(--color-text-muted)', marginBottom: 1, letterSpacing: '0.04em', textTransform: 'uppercase' }}>
          {t('server_status.title')}
        </div>
        <ServerItem label={t('server_status.llm')} status={status.llm} statusLabel={statusLabel(status.llm)} />
        <ServerItem label={t('server_status.embedding')} status={status.embedding} statusLabel={statusLabel(status.embedding)} />
        <ServerItem label={t('server_status.whisper')} status={status.whisper} statusLabel={statusLabel(status.whisper)} />
      </div>
    </>
  )
}
