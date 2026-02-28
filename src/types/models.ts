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
