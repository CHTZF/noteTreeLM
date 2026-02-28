import { useEffect, useRef } from 'react'
import cytoscape, { Core } from 'cytoscape'
import fcose from 'cytoscape-fcose'
import { useGraphStore } from '../../stores/graphStore'
import { useEditorStore } from '../../stores/editorStore'
import { useSettingsStore } from '../../stores/settingsStore'

cytoscape.use(fcose)

const NODE_COLORS: Record<string, string> = {
  note: '#7c8cf8',
  url: '#4ec9b0',
  image: '#d19a66',
  topic: '#e06c75',
}

interface GraphViewProps {
  onOpenNote: (path: string) => void
}

export default function GraphView({ onOpenNote }: GraphViewProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const cyRef = useRef<Core | null>(null)
  const { graphData, load } = useGraphStore()
  const { currentPath } = useEditorStore()
  const { settings } = useSettingsStore()

  // Keep a ref of graphData so the theme-init effect can seed the graph
  const graphDataRef = useRef(graphData)
  useEffect(() => { graphDataRef.current = graphData }, [graphData])

  useEffect(() => { load() }, [])

  // Recreate Cytoscape when theme changes so colors are correct
  useEffect(() => {
    if (!containerRef.current) return

    const getVar = (name: string) =>
      getComputedStyle(document.documentElement).getPropertyValue(name).trim()

    const cy = cytoscape({
      container: containerRef.current,
      style: [
        {
          selector: 'node',
          style: {
            'background-color': (ele: any) => NODE_COLORS[ele.data('node_type')] || '#7c8cf8',
            'label': 'data(label)',
            'color': getVar('--color-text-secondary') || '#8b8fa8',
            'font-size': `${settings.graph_font_size || 11}px`,
            'text-valign': 'bottom',
            'text-halign': 'center',
            'text-margin-y': 4,
            'width': (ele: any) => Math.max(16, Math.min(40, 16 + (ele.data('link_count') || 0) * 2)),
            'height': (ele: any) => Math.max(16, Math.min(40, 16 + (ele.data('link_count') || 0) * 2)),
          },
        },
        {
          selector: 'edge',
          style: {
            'line-color': getVar('--color-border') || '#373a40',
            'width': 1,
            'curve-style': 'bezier',
            'opacity': 0.6,
          },
        },
        {
          selector: 'node.highlighted',
          style: {
            'border-width': 2,
            'border-color': '#7c8cf8',
          },
        },
      ],
      layout: { name: 'fcose', animate: false } as any,
      elements: [],
    })

    cy.on('tap', 'node', (evt) => {
      const node = evt.target
      if (node.data('node_type') === 'note') {
        onOpenNote(node.id())
      }
    })

    // Seed with current data so graph isn't empty after theme switch
    const data = graphDataRef.current
    if (data.nodes.length > 0) {
      cy.add([
        ...data.nodes.map((n) => ({
          group: 'nodes' as const,
          data: { id: n.id, label: n.label, node_type: n.node_type, link_count: n.link_count },
        })),
        ...data.edges.map((e) => ({
          group: 'edges' as const,
          data: { id: String(e.id), source: e.source_id, target: e.target_id },
        })),
      ])
      cy.layout({ name: 'fcose', animate: false } as any).run()
    }

    cyRef.current = cy
    return () => { cy.destroy(); cyRef.current = null }
  }, [settings.theme])

  // 圖譜字級：僅更新樣式，不重建實例
  useEffect(() => {
    const cy = cyRef.current
    if (!cy) return
    cy.style()
      .selector('node')
      .style('font-size', `${settings.graph_font_size || 11}px`)
      .update()
  }, [settings.graph_font_size])

  // 更新資料
  useEffect(() => {
    const cy = cyRef.current
    if (!cy) return

    cy.elements().remove()

    const elements: cytoscape.ElementDefinition[] = [
      ...graphData.nodes.map((n) => ({
        group: 'nodes' as const,
        data: { id: n.id, label: n.label, node_type: n.node_type, link_count: n.link_count },
      })),
      ...graphData.edges.map((e) => ({
        group: 'edges' as const,
        data: { id: String(e.id), source: e.source_id, target: e.target_id },
      })),
    ]

    cy.add(elements)
    cy.layout({ name: 'fcose', animate: true, animationDuration: 500 } as any).run()
  }, [graphData])

  // 高亮當前筆記
  useEffect(() => {
    const cy = cyRef.current
    if (!cy || !currentPath) return
    cy.nodes().removeClass('highlighted')
    const node = cy.getElementById(currentPath)
    if (node.length > 0) {
      node.addClass('highlighted')
      cy.animate({ fit: { eles: node, padding: 80 }, duration: 400 })
    }
  }, [currentPath])

  return (
    <div ref={containerRef} style={{ flex: 1, background: 'var(--color-bg-base)' }} />
  )
}
