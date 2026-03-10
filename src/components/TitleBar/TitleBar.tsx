interface TitleBarProps {
  title?: string
}

export default function TitleBar({ title }: TitleBarProps) {
  return (
    <div
      data-tauri-drag-region
      className="app-titlebar"
    >
      <span className="app-titlebar-title" data-tauri-drag-region>
        {title || 'noteTreeLM'}
      </span>
    </div>
  )
}
