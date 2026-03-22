import { create } from 'zustand'

// ── Types ─────────────────────────────────────────────────────────────────────

export type ActivitySource = 'wikilink' | 'filetree' | 'chat' | 'tab' | 'quickopen' | 'unknown'

export interface ActivityAction {
  type: 'note_open' | 'wikilink_click' | 'selection' | 'copy' | 'pane_focus'
  path?: string       // 目標筆記路徑
  fromPath?: string   // wikilink 來源筆記路徑
  source?: ActivitySource
  text?: string       // selection / copy 的文字內容（max 500 chars）
  paneId?: string
  ts: number          // Date.now()
}

export interface ActivePattern {
  signature: string
  score: number
}

interface ActivityStore {
  // ── State ───────────────────────────────────────────────────────────────
  actions: ActivityAction[]   // 最近 20 個操作，ring buffer
  allOpenPaths: string[]      // 所有 pane 當前開啟的路徑快照
  lastCopied: string | null   // 最近一次複製的文字

  // ── Predictive Mode ──────────────────────────────────────────────────────
  /** 目前偵測到的高信心 pattern；null = 非預測模式 */
  activePattern: ActivePattern | null
  /** 本 session 已執行每日衰減（只跑一次） */
  decayDoneToday: boolean

  // ── Actions ─────────────────────────────────────────────────────────────
  addAction: (action: Omit<ActivityAction, 'ts'>) => void
  setOpenPaths: (paths: string[]) => void
  setPredictiveMode: (pattern: ActivePattern) => void
  clearPredictiveMode: () => void
  markDecayDone: () => void

  // ── Context ─────────────────────────────────────────────────────────────
  /** 格式化成 LLM 可讀的 context 字串，注入 invoke_live_chat / invoke_agent */
  getContextString: () => string
}

const MAX_ACTIONS = 20

// ── Store ─────────────────────────────────────────────────────────────────────

export const useActivityStore = create<ActivityStore>((set, get) => ({
  actions: [],
  allOpenPaths: [],
  lastCopied: null,
  activePattern: null,
  decayDoneToday: false,

  addAction: (action) => {
    const full: ActivityAction = { ...action, ts: Date.now() }
    set(s => {
      const next = [...s.actions, full]
      // ring buffer: 超過 MAX_ACTIONS 時丟棄最舊的
      const trimmed = next.length > MAX_ACTIONS ? next.slice(next.length - MAX_ACTIONS) : next
      return {
        actions: trimmed,
        lastCopied: action.type === 'copy' ? (action.text ?? s.lastCopied) : s.lastCopied,
      }
    })
  },

  setOpenPaths: (paths) => set({ allOpenPaths: paths }),

  setPredictiveMode: (pattern) => set({ activePattern: pattern }),
  clearPredictiveMode: () => set({ activePattern: null }),
  markDecayDone: () => set({ decayDoneToday: true }),

  getContextString: () => {
    const { actions, allOpenPaths, lastCopied, activePattern } = get()
    const lines: string[] = []

    // 工作區快照
    if (allOpenPaths.length > 0) {
      lines.push('[工作區]')
      allOpenPaths.forEach((p, i) => {
        const label = p.startsWith('__') ? `[${p.replace(/__/g, '')}]` : p
        lines.push(`Pane ${i + 1}: ${label}`)
      })
    }

    // 最近操作序列（只取最近 15 分鐘內的）
    const cutoff = Date.now() - 15 * 60 * 1000
    const recent = actions.filter(a => a.ts >= cutoff)
    if (recent.length > 0) {
      lines.push('')
      lines.push('[最近操作]')
      recent.forEach(a => {
        const time = new Date(a.ts).toLocaleTimeString('zh-TW', { hour: '2-digit', minute: '2-digit', second: '2-digit' })
        switch (a.type) {
          case 'wikilink_click':
            lines.push(`${time} wikilink → ${a.path ?? ''}${a.fromPath ? `（from ${a.fromPath}）` : ''}`)
            break
          case 'note_open':
            lines.push(`${time} 開啟 ${a.path ?? ''}（${a.source ?? 'unknown'}）`)
            break
          case 'pane_focus':
            lines.push(`${time} 切換焦點 → ${a.path ?? ''}`)
            break
          case 'selection':
            lines.push(`${time} 框選「${a.text ?? ''}」`)
            break
          case 'copy':
            lines.push(`${time} 複製「${a.text ?? ''}」`)
            break
        }
      })
    }

    // 最近複製（獨立強調）
    const selActions = actions.filter(a => a.type === 'selection')
    const lastSel = selActions[selActions.length - 1]
    if (lastCopied && lastCopied !== lastSel?.text) {
      lines.push('')
      lines.push(`[最近複製]`)
      lines.push(`「${lastCopied}」`)
    }

    // 預測模式提示
    if (activePattern) {
      lines.push('')
      lines.push(`[預測模式] 偵測到行為模式「${activePattern.signature}」(信心: ${(activePattern.score * 100).toFixed(0)}%)`)
    }

    return lines.join('\n')
  },
}))
