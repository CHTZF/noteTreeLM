export interface Settings {
  vault_path: string
  theme: 'dark' | 'light'
  auto_save_mode: 'off' | 'afterDelay' | 'onFocusChange' | 'onWindowChange'
  auto_save_delay: number
  whisper_cli_path: string
  whisper_model_path: string
  whisper_language: string
  whisper_auto_insert: boolean
  import_max_depth: number
  import_max_pages: number
  ai_provider: string
  ai_model: string
  ai_base_url: string
  ai_enable_topics: boolean
  ai_enable_summary: boolean
  ai_enable_vision: boolean
  llm_model_path: string
  llama_cli_path: string
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
  enable_chat: boolean
  llama_server_port: number
  whisper_server_port: number
}

export const DEFAULT_SETTINGS: Settings = {
  vault_path: '',
  theme: 'dark',
  auto_save_mode: 'afterDelay',
  auto_save_delay: 1000,
  whisper_cli_path: '',
  whisper_model_path: '',
  whisper_language: 'auto',
  whisper_auto_insert: true,
  import_max_depth: 3,
  import_max_pages: 50,
  ai_provider: '',
  ai_model: 'gpt-4o',
  ai_base_url: 'https://api.openai.com/v1',
  ai_enable_topics: true,
  ai_enable_summary: true,
  ai_enable_vision: true,
  llm_model_path: '',
  llama_cli_path: '',
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
  enable_chat: false,
  llama_server_port: 8080,
  whisper_server_port: 8081,
}
