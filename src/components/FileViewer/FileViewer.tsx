import { useEffect, useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { convertFileSrc } from '@tauri-apps/api/core'
import { useSettingsStore } from '../../stores/settingsStore'
import { useVaultStore } from '../../stores/vaultStore'

interface FileViewerProps {
  path: string  // relative path from vault root
}

const IMAGE_EXTS = ['jpg', 'jpeg', 'png', 'gif', 'webp', 'svg', 'bmp', 'ico']
const AUDIO_EXTS = ['mp3', 'wav', 'ogg', 'flac', 'm4a', 'aac']
const VIDEO_EXTS = ['mp4', 'mkv', 'avi', 'mov', 'webm']
const PDF_EXTS = ['pdf']
const TEXT_EXTS = [
  'txt', 'json', 'yaml', 'yml', 'toml', 'csv', 'xml',
  'html', 'css', 'js', 'ts', 'jsx', 'tsx', 'sh', 'bash',
  'py', 'rb', 'go', 'rs', 'java', 'c', 'cpp', 'h', 'hpp',
  'swift', 'kt', 'lua', 'r', 'sql', 'env', 'gitignore',
]

export default function FileViewer({ path }: FileViewerProps) {
  const { settings } = useSettingsStore()
  const { openPathExternally } = useVaultStore()

  const [textContent, setTextContent] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const filename = path.split('/').pop() ?? path
  const ext = filename.split('.').pop()?.toLowerCase() ?? ''
  const absPath = settings.vault_path.replace(/\/$/, '') + '/' + path

  const isImage = IMAGE_EXTS.includes(ext)
  const isAudio = AUDIO_EXTS.includes(ext)
  const isVideo = VIDEO_EXTS.includes(ext)
  const isPdf = PDF_EXTS.includes(ext)
  const isText = TEXT_EXTS.includes(ext)

  const assetUrl = convertFileSrc(absPath)

  useEffect(() => {
    if (!isText) { setTextContent(null); return }
    setLoading(true)
    setError(null)
    invoke<string>('read_file_base64', { path: absPath })
      .then((b64) => {
        const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0))
        setTextContent(new TextDecoder().decode(bytes))
      })
      .catch((e) => setError(String(e)))
      .finally(() => setLoading(false))
  }, [absPath, isText])

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', background: 'var(--color-bg-base)' }}>
      {/* Header */}
      <div style={{
        display: 'flex', alignItems: 'center', gap: 8,
        padding: '8px 16px',
        borderBottom: '1px solid var(--color-border)',
        flexShrink: 0,
      }}>
        <span style={{ flex: 1, fontWeight: 600, color: 'var(--color-text-primary)', fontSize: 14, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {filename}
        </span>
        <button
          onClick={() => openPathExternally(absPath).catch(() => {})}
          style={{
            padding: '4px 10px', fontSize: 12,
            background: 'var(--color-bg-hover)',
            color: 'var(--color-text-secondary)',
            border: '1px solid var(--color-border)',
            borderRadius: 5, cursor: 'pointer', flexShrink: 0,
          }}
        >
          在系統開啟
        </button>
      </div>

      {/* Content */}
      <div style={{
        flex: 1, overflow: 'auto',
        display: 'flex',
        alignItems: (isImage || isAudio || isVideo || isPdf || (!isText && !isImage && !isAudio && !isVideo && !isPdf)) ? 'center' : 'stretch',
        justifyContent: (isImage || (!isText && !isImage && !isAudio && !isVideo && !isPdf)) ? 'center' : 'stretch',
        flexDirection: 'column',
        padding: isText ? 0 : 16,
      }}>
        {isImage && (
          <img
            src={assetUrl}
            alt={filename}
            style={{ maxWidth: '100%', maxHeight: '100%', objectFit: 'contain' }}
          />
        )}

        {isAudio && (
          <audio controls src={assetUrl} style={{ width: '100%', maxWidth: 480 }} />
        )}

        {isVideo && (
          <video controls src={assetUrl} style={{ maxWidth: '100%', maxHeight: '100%' }} />
        )}

        {isPdf && (
          <iframe
            src={assetUrl}
            style={{ width: '100%', height: '100%', border: 'none', flex: 1 }}
            title={filename}
          />
        )}

        {isText && loading && (
          <span style={{ color: 'var(--color-text-muted)', padding: 16 }}>載入中…</span>
        )}
        {isText && error && (
          <span style={{ color: 'var(--color-error, #e06c75)', padding: 16 }}>{error}</span>
        )}
        {isText && !loading && textContent !== null && (
          <pre style={{
            flex: 1, margin: 0, padding: 16,
            fontFamily: 'var(--font-mono)',
            fontSize: 13, lineHeight: 1.6,
            overflow: 'auto',
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-all',
            color: 'var(--color-text-primary)',
          }}>
            {textContent}
          </pre>
        )}

        {!isImage && !isAudio && !isVideo && !isPdf && !isText && (
          <div style={{ textAlign: 'center', color: 'var(--color-text-muted)' }}>
            <div style={{ fontSize: 48, marginBottom: 12 }}>📄</div>
            <div style={{ marginBottom: 12 }}>{filename}</div>
            <button
              onClick={() => openPathExternally(absPath).catch(() => {})}
              style={{
                padding: '6px 14px', fontSize: 13,
                background: 'var(--color-accent)',
                color: '#fff',
                border: 'none',
                borderRadius: 6, cursor: 'pointer',
              }}
            >
              在系統開啟
            </button>
          </div>
        )}
      </div>
    </div>
  )
}
