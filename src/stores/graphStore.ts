import { create } from 'zustand'
import { invoke } from '@tauri-apps/api/core'
import { GraphData, GraphNode, GraphEdge } from '../types/models'

interface GraphStore {
  graphData: GraphData
  highlightedNodeId: string | null
  isLoading: boolean
  load: () => Promise<void>
  highlightNode: (id: string | null) => void
  addNode: (node: GraphNode) => void
  removeNode: (id: string) => void
  addEdge: (edge: GraphEdge) => void
  removeEdge: (id: number) => void
}

export const useGraphStore = create<GraphStore>((set, _get) => ({
  graphData: { nodes: [], edges: [] },
  highlightedNodeId: null,
  isLoading: false,

  load: async () => {
    set({ isLoading: true })
    try {
      const graphData = await invoke<GraphData>('get_graph')
      set({ graphData })
    } finally {
      set({ isLoading: false })
    }
  },

  highlightNode: (id) => set({ highlightedNodeId: id }),

  addNode: (node) =>
    set((state) => ({
      graphData: {
        ...state.graphData,
        nodes: [...state.graphData.nodes.filter((n) => n.id !== node.id), node],
      },
    })),

  removeNode: (id) =>
    set((state) => ({
      graphData: {
        nodes: state.graphData.nodes.filter((n) => n.id !== id),
        edges: state.graphData.edges.filter(
          (e) => e.source_id !== id && e.target_id !== id
        ),
      },
    })),

  addEdge: (edge) =>
    set((state) => ({
      graphData: {
        ...state.graphData,
        edges: [...state.graphData.edges.filter((e) => e.id !== edge.id), edge],
      },
    })),

  removeEdge: (id) =>
    set((state) => ({
      graphData: {
        ...state.graphData,
        edges: state.graphData.edges.filter((e) => e.id !== id),
      },
    })),
}))
