export interface Note {
  path: string
  title: string
  content: string
  frontmatter?: string
  word_count: number
  created_at: number
  modified_at: number
}

export interface Link {
  id: number
  source_path: string
  target_title: string
  target_path?: string
  link_type: 'wikilink' | 'url_ref' | 'image_embed'
  raw_text: string
  alias?: string
  heading?: string
  line_number: number
}

export interface GraphNode {
  id: string
  node_type: 'note' | 'url' | 'image' | 'topic'
  label: string
  url?: string
  file_path?: string
  link_count: number
}

export interface GraphEdge {
  id: number
  source_id: string
  target_id: string
  edge_type: 'wikilink' | 'url_ref' | 'image_embed' | 'topic_member' | 'source_import'
  weight: number
}

export interface GraphData {
  nodes: GraphNode[]
  edges: GraphEdge[]
}

export interface SearchResult {
  path: string
  title: string
  snippet: string
  score: number
  source_url?: string  // set when result is from knowledge import (imports/ path)
}

export interface DeleteResult {
  affected_links: number
}

export interface RenameResult {
  new_path: string
  updated_files: string[]
}

export interface ImportResult {
  note_path: string
  title: string
  was_duplicate: boolean
}

export interface TranscribeResult {
  text: string
}

export interface Asset {
  file_path: string
  mime_type: string
  file_size: number
  created_at: number
}

export interface TrashItem {
  id: string
  original_path: string
  name: string
  title: string
  trash_filename: string
  deleted_at: number
}

export interface FileTreeNode {
  name: string
  path: string
  isFolder: boolean
  isImage?: boolean
  children?: FileTreeNode[]
  note?: Note
}

// ── Agent Skills ──────────────────────────────────────────────────────────────

export type AgentScope = 'all' | 'main' | 'search' | 'write' | 'research' | 'memory'

/** LLM 建議的技能規範（尚未持久化，從 suggest_kb_cards_for_item 回傳） */
export interface AgentSkillSuggestion {
  title: string
  trigger: string
  behavior: string
  tool_calls: string[]
  injection_mode?: 'passive' | 'active'
  agent_scope?: AgentScope
  need_tool_chain?: boolean
  tool_chain_order?: string[]
}

/** 持久化後的技能規範（從 DB 讀取） */
export interface AgentSkill {
  skill_id: string
  vault_id: string
  knowledge_item_id: string
  title: string
  trigger: string
  behavior: string
  tool_calls: string[]
  is_active: boolean
  injection_mode: 'passive' | 'active'  // 被動取用 | 主動注入
  agent_scope: AgentScope               // 適用的 agent 範圍
  need_tool_chain: boolean
  tool_chain_order: string[]
  trigger_count: number
  last_triggered_at: number | null  // ms timestamp，null = 從未觸發
  created_at: number
}
