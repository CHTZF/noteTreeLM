/**
 * usePatternDetector
 *
 * Phase 2 — 行為模式學習系統
 *
 * 規則型 pattern 偵測：
 *   - wikilink_chain  ：5 分鐘內連續 ≥2 次 wikilink_click
 *   - compare_notes   ：2 分鐘內開啟 ≥2 個不同筆記並有 selection
 *   - deep_read       ：同一筆記內 ≥3 次 selection（深度閱讀）
 *
 * 每次偵測到 pattern：
 *   1. save_pattern（upsert）
 *   2. list_patterns → 查出 score
 *   3. score > 0.35 → setPredictiveMode
 *   4. 啟動 30s 靜音計時器：
 *      - 使用者說話（外部呼叫 notifySpeak）→ Bayesian +0.2*(1-s)
 *      - 30s 超時                            → Bayesian -0.1*s
 *
 * 每日衰減（每次 App 啟動跑一次）：
 *   decay_patterns × 0.95，score < 0.2 → deprecated
 *
 * 非同步 embedding tagging（P2-9）：
 *   pattern 命中且 semantic_intent 尚未設定時，
 *   用 context snapshot 呼叫 call_external_ai → set_pattern_intent
 */

import { useEffect, useRef, useCallback } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { useActivityStore } from '../stores/activityStore'

// ── Constants ─────────────────────────────────────────────────────────────────

const WIKILINK_WINDOW_MS  = 5 * 60 * 1000   // 5 分鐘
const COMPARE_WINDOW_MS   = 2 * 60 * 1000   // 2 分鐘
const DEEP_READ_THRESHOLD = 3               // 同篇 selection 次數
const PREDICTIVE_THRESHOLD = 0.35           // 進入預測模式的最低 score
const SILENCE_TIMEOUT_MS  = 30_000          // 30 秒靜音 → negative feedback
const EMBEDDING_SCORE_THRESHOLD = 0.5       // 觸發非同步 embedding 的最低 score

// ── Hook ──────────────────────────────────────────────────────────────────────

export function usePatternDetector() {
  const actions           = useActivityStore(s => s.actions)
  const activePattern     = useActivityStore(s => s.activePattern)
  const decayDoneToday    = useActivityStore(s => s.decayDoneToday)
  const setPredictiveMode = useActivityStore(s => s.setPredictiveMode)
  const clearPredictiveMode = useActivityStore(s => s.clearPredictiveMode)
  const markDecayDone     = useActivityStore(s => s.markDecayDone)

  const silenceTimerRef       = useRef<ReturnType<typeof setTimeout> | null>(null)
  const activePatternSigRef   = useRef<string | null>(null)
  const vaultId               = useRef<string>('')

  // Keep vaultId in sync with settings (read once at mount, vault changes are rare)
  useEffect(() => {
    import('../stores/settingsStore').then(({ useSettingsStore }) => {
      vaultId.current = useSettingsStore.getState().settings.system_current_vault_path ?? ''
    })
  }, [])

  // ── Daily Decay (run once per session) ────────────────────────────────────
  useEffect(() => {
    if (decayDoneToday || !vaultId.current) return
    invoke('decay_patterns', { vaultId: vaultId.current }).catch(() => {})
    markDecayDone()
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [decayDoneToday])

  // ── clearSilenceTimer ─────────────────────────────────────────────────────
  const clearSilenceTimer = useCallback(() => {
    if (silenceTimerRef.current) {
      clearTimeout(silenceTimerRef.current)
      silenceTimerRef.current = null
    }
  }, [])

  // ── onPatternMatched: upsert → query score → maybe enter predictive mode ──
  const onPatternMatched = useCallback(async (signature: string) => {
    const vid = vaultId.current
    if (!vid) return

    // 1. Upsert (increments trigger_count)
    await invoke('save_pattern', { vaultId: vid, signature }).catch(() => {})

    // 2. Fetch current score
    type PatternRow = { signature: string; score: number; semantic_intent: string | null }
    const patterns = await invoke<PatternRow[]>('list_patterns', { vaultId: vid, minScore: 0.0 }).catch(() => [] as PatternRow[])
    const matched = patterns.find(p => p.signature === signature)
    const score = matched?.score ?? 0.3

    // 3. Enter predictive mode if score crosses threshold
    if (score >= PREDICTIVE_THRESHOLD) {
      clearSilenceTimer()
      setPredictiveMode({ signature, score })
      activePatternSigRef.current = signature

      // 4. Start 30s silence timer → negative feedback
      silenceTimerRef.current = setTimeout(() => {
        silenceTimerRef.current = null
        if (activePatternSigRef.current === signature) {
          invoke('update_pattern_score', { vaultId: vid, signature, spoke: false }).catch(() => {})
          clearPredictiveMode()
          activePatternSigRef.current = null
        }
      }, SILENCE_TIMEOUT_MS)
    }

    // 5. Async embedding tagging (P2-9) — fire-and-forget when no intent yet and score is high enough
    if (score >= EMBEDDING_SCORE_THRESHOLD && !matched?.semantic_intent) {
      const contextSnap = useActivityStore.getState().getContextString()
      invoke<string>('process_with_llm', {
        prompt: `下列是使用者的操作紀錄，請用5個字以內描述使用者此刻的行為意圖（例如「查詢餐廳菜單」）：\n\n${contextSnap}`,
        maxTokens: 20,
      }).then(intent => {
        if (intent?.trim()) {
          invoke('set_pattern_intent', { vaultId: vid, signature, intent: intent.trim() }).catch(() => {})
        }
      }).catch(() => {})
    }
  }, [clearSilenceTimer, setPredictiveMode, clearPredictiveMode])

  // ── notifySpeak: called by LiveChatSheet when user speaks ─────────────────
  const notifySpeak = useCallback(() => {
    const sig = activePatternSigRef.current
    const vid = vaultId.current
    if (!sig || !vid) return

    clearSilenceTimer()
    invoke('update_pattern_score', { vaultId: vid, signature: sig, spoke: true }).catch(() => {})
    clearPredictiveMode()
    activePatternSigRef.current = null
  }, [clearSilenceTimer, clearPredictiveMode])

  // ── Pattern Detection: watch actions ──────────────────────────────────────
  useEffect(() => {
    if (actions.length === 0) return
    const now = Date.now()

    // ── 1. wikilink_chain: ≥2 wikilink_click within WIKILINK_WINDOW_MS ───
    const recentWikilinks = actions.filter(
      a => a.type === 'wikilink_click' && now - a.ts <= WIKILINK_WINDOW_MS
    )
    if (recentWikilinks.length >= 2) {
      const depth = recentWikilinks.length
      const sig = `wikilink_chain:depth=${depth}`
      onPatternMatched(sig)
    }

    // ── 2. compare_notes: ≥2 distinct paths opened + any selection ────────
    const recentOpens = actions.filter(
      a => (a.type === 'note_open' || a.type === 'wikilink_click') &&
           now - a.ts <= COMPARE_WINDOW_MS &&
           a.path
    )
    const distinctPaths = new Set(recentOpens.map(a => a.path))
    const hasSelection  = actions.some(a => a.type === 'selection' && now - a.ts <= COMPARE_WINDOW_MS)
    if (distinctPaths.size >= 2 && hasSelection) {
      onPatternMatched('compare_notes')
    }

    // ── 3. deep_read: ≥DEEP_READ_THRESHOLD selections within 10 min ─────────
    const recentSel = actions.filter(
      a => a.type === 'selection' && now - a.ts <= 10 * 60 * 1000
    )
    if (recentSel.length >= DEEP_READ_THRESHOLD) {
      onPatternMatched('deep_read')
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [actions])

  // Cleanup on unmount
  useEffect(() => () => clearSilenceTimer(), [clearSilenceTimer])

  return { activePattern, notifySpeak }
}
