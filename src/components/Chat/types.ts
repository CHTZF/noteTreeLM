export interface Message {
  role: 'user' | 'assistant' | 'tool' | 'notice' | 'think'
  content: string
  webRefs?: Array<{ path: string; title: string; excerpt: string }>
  savedWeb?: boolean
}

export type SkillPreview = {
  userMsg: string
  assistantMsg: string
  loading: boolean
  title: string
  trigger: string
  behavior: string
  toolCalls: string[]
  injectionMode: 'passive' | 'active'
}

export type DraftState = { input: string; noteSuggestions: { absPath: string; label: string }[] }

export const ORCHESTRATOR_SYSTEM =
  `你是一個筆記助理，可以直接使用工具完成使用者的請求。\n` +
  `可用工具：\n` +
  `- 讀取：search_vault、read_note、list_structure、list_notes_in_folder、query_memory、get_current_datetime\n` +
  `- 開啟：open_note\n` +
  `- 寫入：create_note、update_note、append_to_note、create_folder\n` +
  `- 刪除/移動：delete_note、delete_folder、move_note\n` +
  `- 搜尋：web_search\n` +
  `- 排程：schedule_task\n` +
  `- UI：show_toast、ui_action\n` +
  `- 反思：reflect_on_skills\n` +
  `- 對話記錄：list_recent_conversations\n` +
  `規則：\n` +
  `1. 使用者要「打開」某筆記 → 先 search_vault 找到路徑，再 open_note 打開。\n` +
  `2. 使用者要「搜尋」或「查看內容」→ search_vault 再 read_note。\n` +
  `3. 需要即時網路資訊 → web_search。\n` +
  `4. 禁止虛構筆記名稱或路徑；搜尋無結果時直接告知找不到。\n` +
  `5. 純閒聊或解釋概念可不使用工具。`
