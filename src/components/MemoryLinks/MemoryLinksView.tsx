import { useEffect, useRef, useCallback, useState } from 'react'
import cytoscape, { Core, NodeSingular } from 'cytoscape'
import fcose from 'cytoscape-fcose'
import { listen } from '@tauri-apps/api/event'
import { api } from '../../lib/api'
import { useSettingsStore } from '../../stores/settingsStore'

cytoscape.use(fcose)

// ─── Visual constants ─────────────────────────────────────────────────────────

const CATEGORY_COLORS: Record<string, string> = {
  personal:   '#f59e0b',
  preference: '#a78bfa',
  project:    '#34d399',
  rule:       '#f87171',
  general:    '#60a5fa',
}
const NOTE_COLOR    = '#7c8cf8'
const THINK_COLOR   = '#22d3ee'
const SKILL_COLOR   = '#fb923c'
const DIMMED_COLOR  = '#3a3d47'
const FOCUSED_RING  = '#ffffff'

const ANIM_MS = 350
const DELAY_MS = 120
const DMN_TOP_N = 5
const DMN_BREATHE_MS = 2800  // full breath cycle (in → out)

// ─── Types ────────────────────────────────────────────────────────────────────

interface MemoryNode {
  node_id:      string
  node_type:    'memory_fact' | 'note' | 'think' | 'skill'
  label:        string
  category?:    string
  content?:     string
  fact_id?:     string
  file_path?:   string
  inject_count?: number
  temp?:        boolean
}
interface MemoryEdge {
  source_id: string
  target_id: string
}

interface Props {
  onOpenNote: (path: string) => void
}

// ─── Component ────────────────────────────────────────────────────────────────

type SourceFilter = 'all' | 'live_chat' | 'chat'

export default function MemoryLinksView({ onOpenNote }: Props) {
  const containerRef      = useRef<HTMLDivElement>(null)
  const cyRef             = useRef<Core | null>(null)
  const focusedRef        = useRef<Set<string>>(new Set())   // currently focused node_ids
  const dmnIdsRef         = useRef<string[]>([])             // top-N by inject_count
  const dmnTimerRef       = useRef<ReturnType<typeof setInterval> | null>(null)
  const dmnPhaseRef       = useRef<0 | 1>(0)                 // 0=exhale(dim), 1=inhale(bright)
  const tempNodeIdsRef    = useRef<Set<string>>(new Set())   // temp think/skill node ids
  const activeMemoryIdsRef = useRef<string[]>([])            // node_ids from last memory:prefetched
  const [sourceFilter, setSourceFilter] = useState<SourceFilter>('all')
  const sourceFilterRef   = useRef<SourceFilter>('all')
  const { settings }      = useSettingsStore()

  // ── Build Cytoscape elements from API data ──────────────────────────────
  const buildElements = (nodes: MemoryNode[], edges: MemoryEdge[]): cytoscape.ElementDefinition[] => [
    ...nodes.map(n => ({
      group: 'nodes' as const,
      data: {
        id:        n.node_id,
        label:     n.node_type === 'memory_fact'
                     ? `[${n.category}]\n${n.label}`
                     : n.label,
        node_type: n.node_type,
        category:  n.category ?? '',
        fact_id:   n.fact_id  ?? '',
        file_path: n.file_path ?? '',
        content:   n.content  ?? '',
      },
    })),
    ...edges.map((e, i) => ({
      group: 'edges' as const,
      data: { id: `e${i}`, source: e.source_id, target: e.target_id },
    })),
  ]

  // ── DMN: breathing pulse on top-N most injected nodes during standby ────
  const stopDMN = useCallback(() => {
    if (dmnTimerRef.current) {
      clearInterval(dmnTimerRef.current)
      dmnTimerRef.current = null
    }
  }, [])

  const startDMN = useCallback(() => {
    stopDMN()
    const cy = cyRef.current
    if (!cy || dmnIdsRef.current.length === 0) return

    dmnPhaseRef.current = 1
    dmnTimerRef.current = setInterval(() => {
      const cy2 = cyRef.current
      if (!cy2) return
      // Only run when nothing is focused (standby state)
      if (focusedRef.current.size > 0) return

      dmnPhaseRef.current = dmnPhaseRef.current === 0 ? 1 : 0
      const opacity = dmnPhaseRef.current === 1 ? 0.5 : 0.2

      dmnIdsRef.current.forEach(id => {
        const node = cy2.getElementById(id)
        if (node.length === 0) return
        const cat = node.data('category') as string
        const color = CATEGORY_COLORS[cat] ?? FOCUSED_RING
        ;(node as any).animate({
          style: { opacity, 'background-color': color, 'width': 10, 'height': 10 },
        }, { duration: DMN_BREATHE_MS / 2 })
      })
    }, DMN_BREATHE_MS / 2)
  }, [stopDMN])

  // ── Apply focused / neighbor / dimmed styles with animation ─────────────
  const applyFocus = useCallback((
    newFocusedIds: Set<string>,
    animate = true
  ) => {
    const cy = cyRef.current
    if (!cy) return

    // Stop DMN when entering focused state, restart when returning to standby
    if (newFocusedIds.size > 0) {
      stopDMN()
    }

    const oldFocused = focusedRef.current
    focusedRef.current = newFocusedIds

    // Collect 1-hop neighbors of newly focused nodes
    const neighborIds = new Set<string>()
    newFocusedIds.forEach(nid => {
      const node = cy.getElementById(nid)
      node.neighborhood('node').forEach((n: NodeSingular) => { neighborIds.add(n.id()) })
    })
    // Remove focused nodes from neighbor set
    newFocusedIds.forEach(id => neighborIds.delete(id))

    const duration = animate ? ANIM_MS : 0

    cy.nodes().forEach((node: NodeSingular) => {
      const id = node.id()
      const isFocused  = newFocusedIds.has(id)
      const isNeighbor = neighborIds.has(id)
      const wasActive  = oldFocused.has(id)
      const nodeType   = node.data('node_type') as string
      const catColor   = nodeType === 'think' ? THINK_COLOR
                       : nodeType === 'skill' ? SKILL_COLOR
                       : CATEGORY_COLORS[node.data('category')] ?? FOCUSED_RING
      const isNote     = nodeType === 'note'

      if (isFocused) {
        const delay = wasActive ? 0 : DELAY_MS
        node.stop(true)
        ;(node as any).delay(delay).animate({
          style: {
            'background-color':    isNote ? NOTE_COLOR : catColor,
            'width':               52,
            'height':              52,
            'opacity':             1,
            'font-size':           13,
            'text-opacity':        1,
            'border-width':        3,
            'border-color':        FOCUSED_RING,
            'border-opacity':      0.9,
            'z-index':             10,
          },
        }, { duration })
      } else if (isNeighbor) {
        node.stop(true)
        ;(node as any).delay(animate ? DELAY_MS / 2 : 0).animate({
          style: {
            'background-color':    isNote ? NOTE_COLOR : catColor,
            'width':               28,
            'height':              28,
            'opacity':             0.8,
            'font-size':           11,
            'text-opacity':        0.9,
            'border-width':        0,
            'border-opacity':      0,
            'z-index':             5,
          },
        }, { duration })
      } else {
        node.stop(true)
        ;(node as any).animate({
          style: {
            'background-color': DIMMED_COLOR,
            'width':            8,
            'height':           8,
            'opacity':          0.2,
            'font-size':        9,
            'text-opacity':     0,
            'border-width':     0,
            'border-opacity':   0,
            'z-index':          1,
          },
        }, { duration: animate ? ANIM_MS / 2 : 0 })
      }
    })

    // Return to DMN standby if no focus
    if (newFocusedIds.size === 0) {
      setTimeout(() => startDMN(), animate ? ANIM_MS + 100 : 0)
    }

    // Pan & zoom to focused + neighbors after animation
    if (newFocusedIds.size > 0) {
      const idsToFit = [...newFocusedIds, ...neighborIds]
      const eles = idsToFit.reduce((acc, id) => {
        const n = cy.getElementById(id)
        return n.length > 0 ? acc.union(n) : acc
      }, cy.collection())
      if (eles.length > 0) {
        setTimeout(() => {
          ;(cy as any).animate(
            { fit: { eles, padding: 80 } },
            { duration: animate ? ANIM_MS + 80 : 0 }
          )
        }, animate ? DELAY_MS : 0)
      }
    }
  }, [stopDMN, startDMN])

  // ── Initialise Cytoscape ────────────────────────────────────────────────
  useEffect(() => {
    if (!containerRef.current) return

    const getVar = (v: string) =>
      getComputedStyle(document.documentElement).getPropertyValue(v).trim()

    const cy = cytoscape({
      container: containerRef.current,
      style: [
        {
          selector: 'node',
          style: {
            'background-color':  DIMMED_COLOR,
            'label':             'data(label)',
            'color':             getVar('--color-text-secondary') || '#8b8fa8',
            'font-size':         9,
            'text-valign':       'bottom',
            'text-halign':       'center',
            'text-margin-y':     5,
            'text-wrap':         'wrap',
            'text-max-width':    '120px',
            'width':             8,
            'height':            8,
            'opacity':           0.2,
            'text-opacity':      0,
            'border-width':      0,
            'transition-property': 'background-color, width, height, opacity, border-width, border-opacity, font-size, text-opacity',
            'transition-duration': `${ANIM_MS}ms` as any,
          },
        },
        {
          selector: 'edge',
          style: {
            'line-color':    getVar('--color-border') || '#373a40',
            'width':         1,
            'curve-style':   'bezier',
            'opacity':       0.3,
          },
        },
      ],
      layout: { name: 'fcose', animate: false } as any,
      elements: [],
      userZoomingEnabled: true,
      userPanningEnabled: true,
    })

    cy.on('tap', 'node', (evt) => {
      const node = evt.target
      const nodeType = node.data('node_type')
      const nodeId   = node.id() as string

      if (nodeType === 'note') {
        const fp = node.data('file_path') as string
        if (fp) onOpenNote(fp)
      } else if (nodeType === 'memory_fact') {
        // Shift focus to this node
        applyFocus(new Set([nodeId]), true)
      }
    })

    cyRef.current = cy
    return () => { stopDMN(); cy.destroy(); cyRef.current = null }
  }, [settings.theme, applyFocus, onOpenNote])

  // ── Load graph data on mount ─────────────────────────────────────────────
  useEffect(() => {
    api.getMemoryGraph().then(data => {
      const cy = cyRef.current
      if (!cy) return
      const nodes = (data.nodes ?? []) as MemoryNode[]
      const edges = (data.edges ?? []) as MemoryEdge[]
      const elems = buildElements(nodes, edges)
      cy.add(elems)
      cy.layout({ name: 'fcose', animate: false } as any).run()

      // Compute DMN: top-N memory_fact nodes by inject_count
      const topN = nodes
        .filter(n => n.node_type === 'memory_fact' && (n.inject_count ?? 0) > 0)
        .sort((a, b) => (b.inject_count ?? 0) - (a.inject_count ?? 0))
        .slice(0, DMN_TOP_N)
        .map(n => n.node_id)
      dmnIdsRef.current = topN

      // Start all dimmed, then begin DMN breathing
      applyFocus(new Set(), false)
    }).catch(() => {})
  }, [applyFocus, startDMN])

  // ── Keep sourceFilterRef in sync with state ──────────────────────────────
  useEffect(() => { sourceFilterRef.current = sourceFilter }, [sourceFilter])

  // ── Listen for memory:prefetched events ──────────────────────────────────
  useEffect(() => {
    let unlisten: (() => void) | null = null

    listen<{ node_ids: string[]; source: string }>('memory:prefetched', async (event) => {
      const cy = cyRef.current
      if (!cy) return

      // Source filter: skip if not matching selected filter
      const { node_ids, source } = event.payload
      const filter = sourceFilterRef.current
      if (filter !== 'all' && source !== filter) return

      // Empty node_ids → show a "no memory" placeholder temp node
      if (node_ids.length === 0) {
        activeMemoryIdsRef.current = ['temp_no_memory']
        addTempNodes('memory_fact', [{ id: 'temp_no_memory', label: '📭 尚無相關記憶' }])
        return
      }

      activeMemoryIdsRef.current = node_ids

      const incomingIds = new Set(node_ids)

      // Add any new nodes that aren't in graph yet (lazy-add)
      const missingIds = [...incomingIds].filter(id => cy.getElementById(id).length === 0)
      if (missingIds.length > 0) {
        try {
          const data = await api.getMemoryGraph()
          const nodes = (data.nodes ?? []) as MemoryNode[]
          const edges = (data.edges ?? []) as MemoryEdge[]
          const existingIds = new Set(cy.elements().map((e: any) => e.id()))
          const newElems = buildElements(nodes, edges).filter(
            el => !existingIds.has(el.data?.id as string)
          )
          if (newElems.length > 0) {
            cy.add(newElems)
            cy.layout({ name: 'fcose', animate: false, randomize: false } as any).run()
          }
          // Refresh DMN top-N after new data
          const topN = nodes
            .filter(n => n.node_type === 'memory_fact' && (n.inject_count ?? 0) > 0)
            .sort((a, b) => (b.inject_count ?? 0) - (a.inject_count ?? 0))
            .slice(0, DMN_TOP_N)
            .map(n => n.node_id)
          dmnIdsRef.current = topN
        } catch { /* best-effort */ }
      }

      applyFocus(incomingIds, true)
    }).then(fn => { unlisten = fn })

    return () => { unlisten?.() }
  }, [applyFocus])

  // ── Helper: add temp nodes (think/skill) + focus them ────────────────────
  const addTempNodes = useCallback((
    type: 'think' | 'skill' | 'memory_fact',
    items: { id: string; label: string }[],
  ) => {
    const cy = cyRef.current
    if (!cy || items.length === 0) return

    // Remove old temp nodes of the same type (and their edges)
    cy.nodes().filter((n: NodeSingular) => n.data('node_type') === type && n.data('temp')).remove()
    cy.edges().filter((e: any) => e.data('temp') && e.data('id')?.startsWith(`edge_${type}_`)).remove()
    tempNodeIdsRef.current = new Set(
      [...tempNodeIdsRef.current].filter(id => cy.getElementById(id).length > 0)
    )

    const newIds = new Set<string>()
    items.forEach(({ id, label }) => {
      if (cy.getElementById(id).length === 0) {
        cy.add({ group: 'nodes', data: { id, label, node_type: type, temp: true } })
        tempNodeIdsRef.current.add(id)
      }
      newIds.add(id)

      // Connect to active memory nodes (skip for memory_fact type — it IS the anchor)
      if (type !== 'memory_fact') {
        activeMemoryIdsRef.current.forEach(memId => {
          const edgeId = `edge_${type}_${id}_${memId}`
          if (cy.getElementById(edgeId).length === 0 && cy.getElementById(memId).length > 0) {
            cy.add({ group: 'edges', data: { id: edgeId, source: id, target: memId, temp: true } })
          }
        })
      }
    })

    cy.layout({ name: 'fcose', animate: false, randomize: false } as any).run()
    applyFocus(newIds, true)
  }, [applyFocus])

  // ── Listen for agent:think ────────────────────────────────────────────────
  useEffect(() => {
    let unlisten: (() => void) | null = null
    let counter = 0
    listen<{ thought: string }>('agent:think', (e) => {
      counter++
      const id = `temp_think_${counter}`
      const label = `💭 ${e.payload.thought.slice(0, 30)}${e.payload.thought.length > 30 ? '…' : ''}`
      addTempNodes('think', [{ id, label }])
    }).then(fn => { unlisten = fn })
    return () => { unlisten?.() }
  }, [addTempNodes])

  // ── Listen for agent:skills_activated ────────────────────────────────────
  useEffect(() => {
    let unlisten: (() => void) | null = null
    listen<{ titles: string[]; source?: string }>('agent:skills_activated', (e) => {
      const items = e.payload.titles.map((title, i) => ({
        id: `temp_skill_${title.replace(/\s+/g, '_')}_${i}`,
        label: `⚡ ${title}`,
      }))
      addTempNodes('skill', items)
    }).then(fn => { unlisten = fn })
    return () => { unlisten?.() }
  }, [addTempNodes])

  // ── Font size setting sync ────────────────────────────────────────────────
  useEffect(() => {
    const cy = cyRef.current
    if (!cy) return
    cy.style().selector('node').style('font-size', `${settings.graph_font_size || 11}px`).update()
  }, [settings.graph_font_size])

  return (
    <div style={{ flex: 1, overflow: 'hidden', display: 'flex', flexDirection: 'column', position: 'relative' }}>
      {/* Legend */}
      <div
        style={{
          position: 'absolute', top: 12, left: 12, zIndex: 10,
          display: 'flex', gap: 6, alignItems: 'center',
          padding: '4px 10px', borderRadius: 6,
          background: 'var(--color-surface)',
          border: '1px solid var(--color-border)',
          fontSize: 12, color: 'var(--color-text-muted)',
          pointerEvents: 'none',
        }}
      >
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: CATEGORY_COLORS.personal, display: 'inline-block' }} />
        personal
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: CATEGORY_COLORS.preference, display: 'inline-block', marginLeft: 6 }} />
        preference
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: CATEGORY_COLORS.project, display: 'inline-block', marginLeft: 6 }} />
        project
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: CATEGORY_COLORS.rule, display: 'inline-block', marginLeft: 6 }} />
        rule
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: NOTE_COLOR, display: 'inline-block', marginLeft: 6 }} />
        note
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: THINK_COLOR, display: 'inline-block', marginLeft: 6 }} />
        think
        <span style={{ width: 10, height: 10, borderRadius: '50%', background: SKILL_COLOR, display: 'inline-block', marginLeft: 6 }} />
        skill
      </div>
      {/* Source filter */}
      <div
        style={{
          position: 'absolute', top: 12, right: 12, zIndex: 10,
          display: 'flex', alignItems: 'center', gap: 6,
        }}
      >
        <select
          value={sourceFilter}
          onChange={e => setSourceFilter(e.target.value as SourceFilter)}
          style={{
            background: 'var(--color-surface)',
            border: '1px solid var(--color-border)',
            borderRadius: 6,
            color: 'var(--color-text-muted)',
            fontSize: 12,
            padding: '3px 8px',
            cursor: 'pointer',
            outline: 'none',
          }}
        >
          <option value="all">全部</option>
          <option value="live_chat">Live Chat</option>
          <option value="chat">Chat</option>
        </select>
      </div>
      <div ref={containerRef} style={{ flex: 1 }} />
    </div>
  )
}
