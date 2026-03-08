import { useEffect, useRef } from 'react'
import cytoscape, { Core } from 'cytoscape'
import fcose from 'cytoscape-fcose'
import { useGraphStore } from '../../stores/graphStore'
import { useEditorStore } from '../../stores/editorStore'
import { useSettingsStore } from '../../stores/settingsStore'
import { useVaultStore } from '../../stores/vaultStore'

cytoscape.use(fcose)

const NODE_COLORS: Record<string, string> = {
  note: '#7c8cf8',
  url: '#4ec9b0',
  image: '#d19a66',
  topic: '#e06c75',
  file: '#98c379',
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
  const { assets } = useVaultStore()

  // Keep refs so theme-init effect can seed the graph with latest data
  const graphDataRef = useRef(graphData)
  const assetsRef = useRef(assets)
  useEffect(() => { graphDataRef.current = graphData }, [graphData])
  useEffect(() => { assetsRef.current = assets }, [assets])

  const buildElements = (data: typeof graphData, assetPaths: string[]): cytoscape.ElementDefinition[] => {
    const existingIds = new Set(data.nodes.map(n => n.id))
    const assetNodes = assetPaths
      .filter(p => !existingIds.has(p))
      .map(p => ({
        group: 'nodes' as const,
        data: { id: p, label: p.split('/').pop() ?? p, node_type: 'file', file_path: p, link_count: 0 },
      }))
    return [
      ...data.nodes.map((n) => ({
        group: 'nodes' as const,
        data: { id: n.id, label: n.label, node_type: n.node_type, link_count: n.link_count, file_path: n.file_path ?? '' },
      })),
      ...data.edges.map((e) => ({
        group: 'edges' as const,
        data: { id: String(e.id), source: e.source_id, target: e.target_id },
      })),
      ...assetNodes,
    ]
  }

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
      const nodeType = node.data('node_type')
      if (nodeType === 'note') {
        onOpenNote(node.id())
      } else if (nodeType === 'file') {
        onOpenNote(node.data('file_path'))
      }
    })

    // Seed with current data so graph isn't empty after theme switch
    const data = graphDataRef.current
    const elems = buildElements(data, assetsRef.current)
    if (elems.length > 0) {
      cy.add(elems)
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

  // 更新資料（graphData 或 assets 變化時重建）
  useEffect(() => {
    const cy = cyRef.current
    if (!cy) return
    cy.elements().remove()
    cy.add(buildElements(graphData, assets))
    cy.layout({ name: 'fcose', animate: true, animationDuration: 500 } as any).run()
  }, [graphData, assets])

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
