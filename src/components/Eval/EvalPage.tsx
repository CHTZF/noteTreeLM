import { useState, useEffect, useCallback } from 'react'
import { api, type EvalCaseSummary, type EvalCaseResult, type EvalSuiteResult, type EvalCaseDetail } from '../../lib/api'

// ── Types ─────────────────────────────────────────────────────────────────────

type RunState = 'idle' | 'running' | 'done'

// ── Helpers ───────────────────────────────────────────────────────────────────

function statusBadge(status: EvalCaseSummary['status']) {
  const map: Record<string, string> = {
    enabled:        'bg-green-100 text-green-800',
    disabled:       'bg-gray-100 text-gray-500',
    pending_review: 'bg-yellow-100 text-yellow-800',
  }
  const label: Record<string, string> = {
    enabled:        '啟用',
    disabled:       '停用',
    pending_review: '待審核',
  }
  return (
    <span className={`px-1.5 py-0.5 rounded text-xs font-medium ${map[status] ?? ''}`}>
      {label[status] ?? status}
    </span>
  )
}

function lastRunBadge(result: EvalCaseSummary['last_run_result']) {
  if (!result) return <span className="text-xs text-gray-400">未執行</span>
  return result === 'passed'
    ? <span className="text-xs text-green-600 font-medium">✓ 通過</span>
    : <span className="text-xs text-red-500 font-medium">✗ 失敗</span>
}

function sourceBadge(source: EvalCaseSummary['source']) {
  return source === 'seed'
    ? <span className="text-xs text-blue-500">seed</span>
    : <span className="text-xs text-purple-500">llm</span>
}

// ── Main Component ────────────────────────────────────────────────────────────

export default function EvalPage() {
  const [cases, setCases]             = useState<EvalCaseSummary[]>([])
  const [loading, setLoading]         = useState(true)
  const [runState, setRunState]       = useState<RunState>('idle')
  const [suiteResult, setSuiteResult] = useState<EvalSuiteResult | null>(null)
  const [analysisState, setAnalysisState] = useState<'idle' | 'running' | 'done'>('idle')
  const [resultMap, setResultMap]     = useState<Record<string, EvalCaseResult>>({})
  const [expandedId, setExpandedId]   = useState<string | null>(null)
  const [detailMap, setDetailMap]     = useState<Record<string, EvalCaseDetail>>({})

  const loadCases = useCallback(async () => {
    try {
      const data = await api.listEvalCases()
      setCases(data.cases)
    } catch (e) {
      console.error('listEvalCases failed', e)
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { loadCases() }, [loadCases])

  // Poll for new proposals while trace_analyst is running in the background.
  // trace_analyst may take 30-60s; a single 3s timeout misses most of the results.
  useEffect(() => {
    if (analysisState !== 'done') return
    // Poll every 8s for up to 2 minutes (15 attempts), then stop.
    let attempts = 0
    const id = setInterval(() => {
      attempts++
      loadCases()
      if (attempts >= 15) clearInterval(id)
    }, 8000)
    return () => clearInterval(id)
  }, [analysisState, loadCases])

  async function handleExpand(caseId: string) {
    if (expandedId === caseId) {
      setExpandedId(null)
      return
    }
    setExpandedId(caseId)
    if (!detailMap[caseId]) {
      try {
        const detail = await api.getEvalCase(caseId)
        setDetailMap(prev => ({ ...prev, [caseId]: detail }))
      } catch (e) {
        console.error('getEvalCase failed', e)
      }
    }
  }

  async function handleDelete(caseId: string) {
    try {
      await api.deleteEvalCase(caseId)
      setCases(prev => prev.filter(c => c.case_id !== caseId))
    } catch (e) {
      console.error('deleteEvalCase failed', e)
    }
  }

  async function handleToggle(caseId: string, currentStatus: EvalCaseSummary['status']) {
    const newEnabled = currentStatus !== 'enabled'
    try {
      await api.toggleEvalCase(caseId, newEnabled)
      setCases(prev => prev.map(c =>
        c.case_id === caseId
          ? { ...c, status: newEnabled ? 'enabled' : 'disabled' }
          : c
      ))
    } catch (e) {
      console.error('toggleEvalCase failed', e)
    }
  }

  async function handleRunAll() {
    setRunState('running')
    setSuiteResult(null)
    setResultMap({})
    try {
      const result = await api.runEvalSuite()
      setSuiteResult(result)
      const map: Record<string, EvalCaseResult> = {}
      for (const r of result.results) map[r.name] = r
      setResultMap(map)
      // Refresh last_run_result in list
      await loadCases()
    } catch (e) {
      console.error('runEvalSuite failed', e)
    } finally {
      setRunState('done')
    }
  }

  async function handleTraceAnalysis() {
    setAnalysisState('running')
    try {
      await api.runTraceAnalysis()
      setAnalysisState('done') // triggers polling useEffect
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e)
      if (msg.includes('409') || msg.includes('already running')) {
        setAnalysisState('idle')
        // Don't log as error — this is expected when button is clicked twice
      } else {
        console.error('runTraceAnalysis failed', e)
        setAnalysisState('idle')
      }
    }
  }

  const enabledCount = cases.filter(c => c.status === 'enabled').length
  const pendingCount = cases.filter(c => c.status === 'pending_review').length

  return (
    <div className="p-6 max-w-4xl mx-auto space-y-6">

      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-xl font-semibold text-gray-900">Eval Suite</h1>
          <p className="text-sm text-gray-500 mt-0.5">
            {cases.length} cases · {enabledCount} 啟用
            {pendingCount > 0 && <span className="ml-2 text-yellow-600">{pendingCount} 待審核</span>}
          </p>
        </div>
        <div className="flex gap-2">
          <button
            onClick={analysisState === 'done' ? () => setAnalysisState('idle') : handleTraceAnalysis}
            disabled={analysisState === 'running'}
            className="px-3 py-1.5 text-sm border border-purple-300 text-purple-700 rounded hover:bg-purple-50 disabled:opacity-50"
          >
            {analysisState === 'running' ? '分析中…' : analysisState === 'done' ? '✓ 已提案（再次分析）' : '分析 Traces'}
          </button>
          <button
            onClick={handleRunAll}
            disabled={runState === 'running' || enabledCount === 0}
            className="px-3 py-1.5 text-sm bg-blue-600 text-white rounded hover:bg-blue-700 disabled:opacity-50"
          >
            {runState === 'running' ? '執行中…' : runState === 'done' ? '再次執行' : 'Run All'}
          </button>
        </div>
      </div>

      {/* Suite summary */}
      {suiteResult && (
        <div className={`rounded-lg p-4 text-sm ${suiteResult.failed === 0 ? 'bg-green-50 border border-green-200' : 'bg-red-50 border border-red-200'}`}>
          <span className="font-medium">
            {suiteResult.failed === 0
              ? `✓ 全部通過 (${suiteResult.passed}/${suiteResult.total})`
              : `✗ ${suiteResult.failed} 個失敗 (${suiteResult.passed}/${suiteResult.total} 通過)`}
          </span>
        </div>
      )}

      {/* Case list */}
      {loading ? (
        <p className="text-sm text-gray-400">載入中…</p>
      ) : cases.length === 0 ? (
        <p className="text-sm text-gray-400">尚無 eval cases。點擊「分析 Traces」讓 trace_analyst 提案。</p>
      ) : (
        <div className="space-y-2">
          {cases.map(c => {
            const runResult = resultMap[c.name]
            const isExpanded = expandedId === c.case_id
            const detail = detailMap[c.case_id]
            return (
              <div key={c.case_id} className="border border-gray-200 rounded-lg">
                <div className="p-3">
                  <div className="flex items-start justify-between gap-3">
                    <div className="flex-1 min-w-0">
                      <div className="flex items-center gap-2 flex-wrap">
                        <button
                          onClick={() => handleExpand(c.case_id)}
                          className="text-sm font-medium text-gray-800 hover:text-blue-600 text-left"
                        >
                          {isExpanded ? '▾' : '▸'} {c.name}
                        </button>
                        {sourceBadge(c.source)}
                        {statusBadge(c.status)}
                        {lastRunBadge(c.last_run_result)}
                        {runResult && !runResult.passed && (
                          <span className="text-xs text-red-500">✗ 本次</span>
                        )}
                        {runResult && runResult.passed && (
                          <span className="text-xs text-green-600">✓ 本次</span>
                        )}
                      </div>
                      <p className="text-xs text-gray-400 mt-0.5">{c.step_count} steps</p>
                      {c.description && (
                        <p className="text-xs text-gray-500 mt-1 italic">{c.description}</p>
                      )}
                      {/* Failure details */}
                      {runResult && !runResult.passed && runResult.failures.length > 0 && (
                        <ul className="mt-1.5 space-y-0.5">
                          {runResult.failures.map((f, i) => (
                            <li key={i} className="text-xs text-red-600 font-mono bg-red-50 px-2 py-0.5 rounded">{f}</li>
                          ))}
                        </ul>
                      )}
                    </div>
                    <div className="flex gap-1 shrink-0">
                      <button
                        onClick={() => handleToggle(c.case_id, c.status)}
                        className={`text-xs px-2 py-1 rounded border ${
                          c.status === 'enabled'
                            ? 'border-green-300 text-green-700 hover:bg-green-50'
                            : 'border-gray-300 text-gray-500 hover:bg-gray-50'
                        }`}
                      >
                        {c.status === 'enabled' ? '停用' : '啟用'}
                      </button>
                      {c.source !== 'seed' && (
                        <button
                          onClick={() => handleDelete(c.case_id)}
                          className="text-xs px-2 py-1 rounded border border-red-200 text-red-400 hover:bg-red-50"
                          title="刪除此提案"
                        >
                          ✕
                        </button>
                      )}
                    </div>
                  </div>
                </div>
                {/* Expandable detail: tool_sequence + assertions */}
                {isExpanded && (
                  <div className="border-t border-gray-100 bg-gray-50 rounded-b-lg px-3 py-2 space-y-2">
                    {!detail ? (
                      <p className="text-xs text-gray-400">載入中…</p>
                    ) : (
                      <>
                        <div>
                          <p className="text-xs font-medium text-gray-500 mb-1">tool_sequence</p>
                          <pre className="text-xs bg-white border border-gray-200 rounded p-2 overflow-x-auto">
                            {JSON.stringify(detail.tool_sequence, null, 2)}
                          </pre>
                        </div>
                        <div>
                          <p className="text-xs font-medium text-gray-500 mb-1">assertions</p>
                          <pre className="text-xs bg-white border border-gray-200 rounded p-2 overflow-x-auto">
                            {JSON.stringify(detail.assertions, null, 2)}
                          </pre>
                        </div>
                        {detail.source_trace_ids.length > 0 && (
                          <p className="text-xs text-gray-400">
                            來源 traces: {detail.source_trace_ids.join(', ')}
                          </p>
                        )}
                      </>
                    )}
                  </div>
                )}
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}
