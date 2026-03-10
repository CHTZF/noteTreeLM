export default function HelpPanel() {
  const sections = [
    {
      emoji: '📁',
      title: '筆記管理',
      desc: '在左側檔案樹中建立、重新命名、刪除筆記與資料夾。支援拖曳排序，右鍵可開啟選單。按 ⌘P 快速開啟任意筆記。',
    },
    {
      emoji: '🕸️',
      title: '知識圖譜',
      desc: '點選左側圖譜圖示開啟視覺化知識圖譜。筆記間的 [[連結]] 會自動顯示為節點連線，方便探索知識脈絡。',
    },
    {
      emoji: '💬',
      title: 'AI Chat',
      desc: '開啟 Chat 頁籤與 AI 對話。AI 可讀取、建立、搜尋你的筆記，並根據你的工作區知識回答問題。',
    },
    {
      emoji: '🎙️',
      title: '語音輸入',
      desc: '開啟 Live Chat 頁籤，使用語音與 AI 即時對話。需在設定中配置 Whisper 語音辨識伺服器。',
    },
    {
      emoji: '🗄️',
      title: '工作區管理',
      desc: '一個工作區對應一個本地資料夾。可建立多個工作區（例如工作、學習、個人），並在工作區之間快速切換。',
    },
    {
      emoji: '⌨️',
      title: '快捷鍵',
      desc: '⌘P 快速開啟　⌘, 開啟設定　⌘[ 上一頁　⌘] 下一頁',
    },
  ]

  return (
    <div style={{ flex: 1, overflowY: 'auto', padding: '24px 28px' }}>
      <div style={{ marginBottom: '24px' }}>
        <h2 style={{ fontSize: '18px', fontWeight: 700, color: 'var(--color-text-primary)', margin: 0, marginBottom: '6px' }}>說明 &amp; 使用指南</h2>
        <p style={{ fontSize: '13px', color: 'var(--color-text-muted)', margin: 0 }}>探索 noteTreeLM 的主要功能</p>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: '14px' }}>
        {sections.map((s) => (
          <div
            key={s.title}
            style={{
              background: 'var(--color-bg-elevated)',
              border: '1px solid var(--color-border)',
              borderRadius: '10px',
              padding: '16px 18px',
            }}
          >
            <div style={{ display: 'flex', alignItems: 'center', gap: '10px', marginBottom: '10px' }}>
              <span style={{ fontSize: '22px' }}>{s.emoji}</span>
              <span style={{ fontSize: '14px', fontWeight: 600, color: 'var(--color-text-primary)' }}>{s.title}</span>
            </div>
            <p style={{ fontSize: '12.5px', color: 'var(--color-text-secondary)', margin: 0, lineHeight: 1.65 }}>{s.desc}</p>
          </div>
        ))}
      </div>
    </div>
  )
}
