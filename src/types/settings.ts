export interface Settings {
  system_current_vault_path: string
  personal_current_vault_path: string
  theme: 'dark' | 'light'
  auto_save_mode: 'off' | 'afterDelay' | 'onFocusChange' | 'onWindowChange'
  auto_save_delay: number
  whisper_cli_path: string
  whisper_model_path: string
  whisper_language: string
  whisper_threads: number
  whisper_auto_insert: boolean
  import_max_depth: number
  import_max_pages: number
  ai_enable_topics: boolean
  ai_enable_summary: boolean
  ai_enable_vision: boolean
  llm_model_path: string
  llama_cli_path: string
  embedding_model_path: string
  last_open_note: string
  onboarding_done: boolean
  recent_vaults: string[]
  sidebar_width: number
  graph_panel_width: number
  sort_orders: Record<string, string[]>
  font_sans: string
  font_mono: string
  editor_font_size: number
  ui_font_size: number
  graph_font_size: number
  debug_mode: boolean
  voice_process_mode: 'none' | 'format' | 'summary'
  voice_preview_enabled: boolean
  voice_noise_suppression: boolean
  voice_preview_interval: number
  enable_chat: boolean
  enable_auto_memory: boolean
  memory_threshold: number
  write_confirm_mode: 'always' | 'once' | 'never'
  chat_auto_include_note: boolean
  display_name: string
  avatar_emoji: string
  avatar_type: 'initials' | 'image'
  avatar_color: string
  avatar_image: string
  show_agent_tools: boolean
  show_spotlight: boolean
  show_graph: boolean
  show_agents: boolean
  show_skills: boolean
  show_kb_assist: boolean
  show_import: boolean
  file_sort_type: 'none' | 'asc' | 'desc'
  hotkey_toggle_view: string
  hotkey_save: string
  hotkey_bold: string
  hotkey_italic: string
}

export const DEFAULT_SETTINGS: Settings = {
  system_current_vault_path: '',
  personal_current_vault_path: '',
  theme: 'dark',
  auto_save_mode: 'afterDelay',
  auto_save_delay: 1000,
  whisper_cli_path: '',
  whisper_model_path: '',
  whisper_language: 'auto',
  whisper_threads: 4,
  whisper_auto_insert: true,
  import_max_depth: 3,
  import_max_pages: 50,
  ai_enable_topics: true,
  ai_enable_summary: true,
  ai_enable_vision: true,
  llm_model_path: '',
  llama_cli_path: '',
  embedding_model_path: '',
  last_open_note: '',
  onboarding_done: false,
  recent_vaults: [],
  sidebar_width: 240,
  graph_panel_width: 320,
  sort_orders: {},
  font_sans: '',
  font_mono: '',
  editor_font_size: 14,
  ui_font_size: 14,
  graph_font_size: 11,
  debug_mode: false,
  voice_process_mode: 'none',
  voice_preview_enabled: true,
  voice_noise_suppression: true,
  voice_preview_interval: 5000,
  enable_chat: false,
  enable_auto_memory: false,
  memory_threshold: 20,
  write_confirm_mode: 'always',
  chat_auto_include_note: false,
  display_name: '',
  avatar_emoji: '',
  avatar_type: 'initials',
  avatar_color: '#0a84ff',
  avatar_image: '',
  show_agent_tools: false,
  show_spotlight: true,
  show_graph: false,
  show_agents: false,
  show_skills: false,
  show_kb_assist: false,
  show_import: false,
  file_sort_type: 'none',
  hotkey_toggle_view: 'mod+e',
  hotkey_save: 'mod+s',
  hotkey_bold: 'mod+b',
  hotkey_italic: 'mod+i',
}
