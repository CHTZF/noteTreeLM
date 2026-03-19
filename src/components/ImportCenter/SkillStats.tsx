import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import {
  LineChart, Line, XAxis, YAxis, Tooltip, ResponsiveContainer,
  ReferenceLine,
} from 'recharts'

// ── Types ─────────────────────────────────────────────────────────────────────

interface DailyCount {
  date: string  // "YYYY-MM-DD"
  count: number
}

interface SkillTrendStat {
  skill_id: string
  title: string
  trigger_count: number
  daily: DailyCount[]
}

interface SkillUsageStats {
  global_daily: DailyCount[]
  top_skills: SkillTrendStat[]
  active_count: number
  total_triggers_30d: number
}

// ── Mini Sparkline (per-skill) ─────────────────────────────────────────────────

export function SkillSparkline({ daily }: { daily: DailyCount[] }) {
  const max = Math.max(...daily.map(d => d.count), 1)
  const total = daily.reduce((s, d) => s + d.count, 0)
  if (total === 0) return <span style={{ fontSize: 10, color: 'var(--color-text-muted)', opacity: 0.5 }}>無記錄</span>

  return (
    <ResponsiveContainer width={80} height={24}>
      <LineChart data={daily} margin={{ top: 2, right: 0, left: 0, bottom: 2 }}>
        <Line
          type="monotone"
          dataKey="count"
          stroke="var(--color-accent, #6366f1)"
          strokeWidth={1.5}
          dot={false}
          isAnimationActive={false}
        />
        {max > 0 && <ReferenceLine y={0} stroke="transparent" />}
      </LineChart>
    </ResponsiveContainer>
  )
}

// ── Global Stats Panel ─────────────────────────────────────────────────────────

export default function SkillStatsPanel() {
  const [stats, setStats] = useState<SkillUsageStats | null>(null)
  const [loading, setLoading] = useState(true)
  const [expanded, setExpanded] = useState(false)

  useEffect(() => {
    invoke<SkillUsageStats>('get_skill_usage_stats')
      .then(setStats)
      .catch(() => {})
      .finally(() => setLoading(false))
  }, [])

  if (loading) return null
  if (!stats) return null

  // 只顯示月份/日期的 tick
  const fmtTick = (v: string) => {
    const d = new Date(v)
    return `${d.getMonth() + 1}/${d.getDate()}`
  }

  const maxCount = Math.max(...stats.global_daily.map(d => d.count), 1)

  return (
    <div className="skill-stats__panel">
      {/* 摘要 header */}
      <div className="skill-stats__summary" onClick={() => setExpanded(v => !v)} style={{ cursor: 'pointer' }}>
        <span className="skill-stats__title">📊 技能統計</span>
        <div className="skill-stats__badges">
          <span className="skill-stats__badge">{stats.active_count} 條啟用中</span>
          <span className="skill-stats__badge accent">近 30 天觸發 {stats.total_triggers_30d} 次</span>
        </div>
        <span className="skill-stats__toggle">{expanded ? '▲' : '▼'}</span>
      </div>

      {expanded && (
        <div className="skill-stats__body">
          {/* 全局折線圖 */}
          <div className="skill-stats__section-label">全局觸發趨勢（過去 30 天）</div>
          <ResponsiveContainer width="100%" height={120}>
            <LineChart data={stats.global_daily} margin={{ top: 4, right: 8, left: -20, bottom: 0 }}>
              <XAxis
                dataKey="date"
                tickFormatter={fmtTick}
                interval={6}
                tick={{ fontSize: 9, fill: 'var(--color-text-muted)' }}
                axisLine={false}
                tickLine={false}
              />
              <YAxis
                allowDecimals={false}
                domain={[0, maxCount + 1]}
                tick={{ fontSize: 9, fill: 'var(--color-text-muted)' }}
                axisLine={false}
                tickLine={false}
                width={24}
              />
              <Tooltip
                contentStyle={{
                  background: 'var(--color-bg-overlay)',
                  border: '1px solid var(--color-border)',
                  borderRadius: 6,
                  fontSize: 11,
                  color: 'var(--color-text)',
                }}
                formatter={(v) => [`${v} 次`, '觸發']}
                labelFormatter={(label) => fmtTick(String(label))}
              />
              <Line
                type="monotone"
                dataKey="count"
                stroke="var(--color-accent, #6366f1)"
                strokeWidth={2}
                dot={false}
                activeDot={{ r: 3 }}
              />
            </LineChart>
          </ResponsiveContainer>

          {/* Top skills 排行 */}
          {stats.top_skills.length > 0 && (
            <>
              <div className="skill-stats__section-label" style={{ marginTop: 12 }}>使用率排行</div>
              <div className="skill-stats__ranking">
                {stats.top_skills.map((sk, i) => {
                  const pct = stats.top_skills[0].trigger_count > 0
                    ? (sk.trigger_count / stats.top_skills[0].trigger_count) * 100
                    : 0
                  return (
                    <div key={sk.skill_id} className="skill-stats__rank-row">
                      <span className="skill-stats__rank-num">{i + 1}</span>
                      <span className="skill-stats__rank-title" title={sk.title}>{sk.title}</span>
                      <div className="skill-stats__rank-bar-wrap">
                        <div className="skill-stats__rank-bar" style={{ width: `${pct}%` }} />
                      </div>
                      <span className="skill-stats__rank-count">{sk.trigger_count}</span>
                      <div className="skill-stats__rank-sparkline">
                        <SkillSparkline daily={sk.daily} />
                      </div>
                    </div>
                  )
                })}
              </div>
            </>
          )}
        </div>
      )}
    </div>
  )
}
