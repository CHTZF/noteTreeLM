/// 內建 seed 資料：每次 Tauri 啟動呼叫 `POST /vaults/:vault_id/seed-builtins`，
/// 幂等重建所有 builtin skills 與 builtin agent definitions。
///
/// 設計原則：
/// - skills：knowledge_item_id = "__builtin__"，injection_mode = "passive"
/// - agents：is_builtin = true
/// - 每次全刪重插，確保 behavior / tool_calls / system_prompt 始終是最新版

use chrono::Utc;
use crate::db::SurrealDb;

struct BuiltinSkill {
    id: &'static str,
    title: &'static str,
    trigger: &'static str,
    behavior: &'static str,
    tools: &'static [&'static str],
    need_tool_chain: bool,
    tool_chain_order: &'static [&'static str],
    injection_mode: &'static str,
}

struct BuiltinAgent {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    kind: &'static str,
    tool_names: &'static [&'static str],
    system_prompt: &'static str,
    max_rounds: i64,
    trigger: &'static str,
}

const SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        id: "builtin_open_note",
        title: "打開/查看筆記",
        trigger: "打開、開啟、open、打開筆記、開啟文件、跳轉到筆記、查看筆記、幫我看某篇、看一下某個、切換到筆記、show me the note、open note、display note、navigate to、go to note、讓我看看、帶我去、開那篇",
        behavior: "步驟1：呼叫 search_vault，傳入使用者提到的筆記名稱作為 query（若包含 .md 副檔名請去除），取得精確的檔案路徑。步驟2：用步驟1取得的路徑呼叫 open_note。步驟3：只回覆「已打開 [筆記名稱]」，絕對不要輸出筆記內容。",
        tools: &["search_vault", "open_note"],
        need_tool_chain: true,
        tool_chain_order: &["search_vault", "open_note"],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_edit_note",
        title: "編輯/修改筆記",
        trigger: "修改、編輯、更新、改、修改筆記、編輯文件、更新內容、改一下某篇、幫我改、修改某段、重寫某節、edit note、update note、revise、幫我更新、把這段改成、把內容換成、調整一下、改掉、替換內容、覆寫",
        behavior: "步驟1：呼叫 search_vault 取得目標筆記的精確路徑。步驟2：呼叫 read_note 讀取完整現有內容。步驟3：呼叫 plan_announce 告知使用者將修改哪個檔案、改什麼內容，deferred_tools 填 update_note。步驟4：使用者確認後，將修改後的完整內容呼叫 update_note 寫入。",
        tools: &["search_vault", "read_note", "update_note", "plan_announce"],
        need_tool_chain: true,
        tool_chain_order: &["search_vault", "read_note", "update_note"],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_create_note",
        title: "新增/建立筆記",
        trigger: "建立、新增、寫、記、建立新筆記、寫一篇、新增文件、記錄下來、幫我新增、建立一份、寫個筆記、create note、new note、write a note、幫我寫、新建一個、記一下、存成筆記、建立文件、新增一篇",
        behavior: "步驟1：若使用者未指定資料夾，先呼叫 list_structure（path 傳空字串）查看現有目錄結構，選擇合適的存放位置。步驟2：呼叫 plan_announce，告知使用者將建立的檔案路徑與內容摘要，deferred_tools 填 create_note。步驟3：使用者確認後，呼叫 create_note（path 格式為 folder/filename.md，content 為完整 Markdown 內容）。",
        tools: &["list_structure", "create_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_append_note",
        title: "追加/補充筆記內容",
        trigger: "在筆記末尾加、補充內容、追加到筆記、把這個加進去、加到筆記裡、繼續寫到某篇、append to note、add to note、在後面加上、補上去、加一段、繼續記錄、把這段加進",
        behavior: "步驟1：呼叫 search_vault 取得目標筆記的精確路徑。步驟2：呼叫 plan_announce，說明要追加的內容，deferred_tools 填 append_to_note。步驟3：使用者確認後，呼叫 append_to_note（path 使用步驟1的路徑，content 為要追加的文字，會自動加在末尾不覆蓋原有內容）。",
        tools: &["search_vault", "append_to_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_search_note",
        title: "搜尋/查詢筆記",
        trigger: "搜尋、找、查、搜尋筆記、找資料、查某個主題、有沒有寫過、幫我找、找找看、查詢相關筆記、search notes、find note、look up、我有沒有寫、我有記過嗎、筆記裡有沒有、幫我查一下、找找有沒有、知識庫裡有",
        behavior: "步驟1：呼叫 search_vault，query 填入使用者問題中的關鍵字，取得相關筆記清單與摘要。步驟2：根據搜尋結果回覆使用者找到哪些筆記。步驟3（可選）：若使用者需要看完整內容，用搜尋結果中的路徑呼叫 read_note；若使用者要打開筆記，呼叫 open_note。",
        tools: &["search_vault", "read_note", "open_note"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_summarize_notes",
        title: "整理/摘要多篇筆記",
        trigger: "整理筆記、摘要多篇、幫我歸納、整合資料、總結所有、彙整、整理一份、summarize notes、consolidate、幫我做個總結、把這些筆記整理、歸納重點、統整一下、彙整成一篇、做個摘要",
        behavior: "步驟1：呼叫 list_notes_in_folder（folder 填目標資料夾路徑）或 search_vault 取得相關筆記清單。步驟2：對清單中每篇筆記呼叫 read_note 讀取完整內容。步驟3：彙整所有內容後輸出摘要。步驟4（可選）：若使用者要存成新筆記，呼叫 plan_announce 確認後再 create_note 建立。",
        tools: &["search_vault", "list_notes_in_folder", "read_note", "create_note", "update_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_browse_structure",
        title: "瀏覽資料夾結構",
        trigger: "看資料夾、瀏覽結構、有什麼筆記、列出所有、Vault 裡有什麼、資料夾有哪些、目錄結構、list folders、show structure、browse vault、我有哪些資料夾、筆記庫裡有什麼、幫我看一下有什麼檔案、目前有哪些筆記",
        behavior: "步驟1：呼叫 list_structure（path 傳空字串表示根目錄）取得整個 Vault 的資料夾與檔案樹狀結構。步驟2：若使用者要看特定資料夾的筆記列表，呼叫 list_notes_in_folder（folder 填資料夾名稱）。步驟3：以清單格式回覆結構內容。",
        tools: &["list_structure", "list_notes_in_folder"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_delete_note",
        title: "刪除筆記",
        trigger: "刪除筆記、移除文件、把某篇刪掉、刪掉這個筆記、清除某個文件、delete note、remove note、trash note、把這篇刪了、幫我刪、不要那篇了、刪除這份文件、清掉",
        behavior: "步驟1：呼叫 search_vault 確認要刪除的筆記精確路徑。步驟2：呼叫 plan_announce，明確告知使用者「將永久刪除 [路徑]，此操作不可復原」，deferred_tools 填 delete_note。步驟3：等使用者明確確認後才呼叫 delete_note。若使用者取消則不執行。",
        tools: &["search_vault", "delete_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_move_note",
        title: "移動/重新命名筆記",
        trigger: "移動筆記、重新命名、換個位置、改檔名、搬移、把筆記移到、重命名、move note、rename note、relocate、把這篇搬到、改個名字、換個名稱、移到另一個資料夾、把文件移過去",
        behavior: "步驟1：呼叫 search_vault 確認來源筆記的精確路徑（from 參數）。步驟2：與使用者確認目標路徑（to 參數，格式如 new_folder/new_name.md）。步驟3：呼叫 plan_announce 說明搬移計畫，deferred_tools 填 move_note。步驟4：使用者確認後呼叫 move_note。",
        tools: &["search_vault", "move_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_organize_folders",
        title: "整理資料夾架構",
        trigger: "建立資料夾、整理目錄、新增分類、重新整理資料夾、建立分類結構、規劃目錄、create folder、organize folders、新增一個資料夾、幫我建個目錄、整理一下分類、建立子目錄、新增分類夾",
        behavior: "步驟1：呼叫 list_structure（path 傳空字串）了解現有資料夾結構。步驟2：根據使用者需求規劃新架構，呼叫 plan_announce 說明整體計畫。步驟3：使用者確認後，依序呼叫 create_folder 建立新資料夾。步驟4（可選）：若需搬移現有筆記，呼叫 move_note 逐一搬移。",
        tools: &["list_structure", "create_folder", "move_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_query_memory",
        title: "查詢過去對話記憶",
        trigger: "之前說過什麼、記憶查詢、歷史對話、上次討論、我之前提到、你記得嗎、過去的對話、recall memory、what did I say、上次我們聊、你還記得、之前提過、我跟你說過、先前的對話、記得我說的",
        behavior: "步驟1：從使用者問題中提取關鍵字（人名、主題、事件等）。步驟2：呼叫 query_memory（keywords 填提取的關鍵字陣列，若無關鍵字則傳空陣列取最新記憶）。步驟3：根據回傳的記憶內容回答使用者。",
        tools: &["query_memory"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_web_search",
        title: "網路搜尋/外部資訊",
        trigger: "搜尋網路、查最新資訊、Google 一下、查新聞、找外部資料、搜網路、最新消息、web search、search the web、look it up online、查天氣、今天天氣、現在幾度、股價、最新匯率、即時資訊、查一下網路、幫我搜尋、去網路上找、外部資訊、最新動態、新聞、時事、最近發生什麼",
        behavior: "步驟1：呼叫 web_search（query 填具體搜尋關鍵字），取得最新網路資訊。步驟2：根據搜尋結果摘要回答使用者。",
        tools: &["web_search"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_create_skill",
        title: "新增/建立技能規範",
        trigger: "新增技能、建立技能、設計技能規範、幫我建立skill、新增skill、新建技能、create skill、design skill、建一個技能、幫我設計一個skill、新增一個agent技能、我要建立技能、建立技能規範、新增行為規則",
        behavior: "步驟1：請使用者描述技能的目的（觸發情境、希望AI執行什麼操作）。步驟2：根據描述，組成以下欄位呼叫 create_agent_skill：title（技能標題）、trigger（觸發語境描述）、behavior（分步驟操作說明）、injection_mode（'passive' 或 'active'）。步驟3：呼叫成功後告知使用者技能已建立，並說明什麼情況下此技能會被觸發。",
        tools: &["create_agent_skill"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_suggest_note_cards",
        title: "AI 建議筆記卡片",
        trigger: "幫我建立筆記卡片、建立知識卡片、整理成卡片、幫我產生筆記卡片、建議筆記卡片、生成卡片、suggest note cards、create note card、知識卡片建議、幫我做成卡片、把這個整理成卡片、幫我萃取卡片、建立 concept 卡片、建立 procedure 卡片、建立 reference 卡片",
        behavior: "步驟1：呼叫 search_vault 或 read_note 取得使用者指定的知識內容（若使用者已提供內容則跳過）。步驟2：分析內容，依 concept、procedure、reference 三種模板各生成 1 張筆記卡片建議，每張包含：標題、模板類型、完整 Markdown 內容（含 frontmatter）、建立理由。步驟3：呼叫 plan_announce 列出將建立的卡片清單，deferred_tools 填 create_note。步驟4：使用者確認後，逐一呼叫 create_note（路徑格式：cards/[標題].md）。",
        tools: &["search_vault", "read_note", "create_note", "plan_announce"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_schedule_task",
        title: "排程/提醒任務",
        trigger: "排程、設定提醒、定時任務、設定鬧鐘、幫我排程、提醒我、到時候提醒、設定時間、定時提醒、schedule、remind me、set reminder、set alarm、幫我設一個提醒、X點提醒我、明天提醒、每天提醒、固定時間、重複執行、定期通知、設定排程任務、每週提醒",
        behavior: "步驟1：呼叫 get_current_datetime 取得現在時間（作為計算相對時間的基準）。步驟2：根據使用者描述確定：（a）任務描述 description、（b）執行時間 run_at（ISO 8601 格式，需含時區，例如 2026-03-22T09:00:00+08:00）、（c）若需重複則填 repeat_interval_seconds（秒數，如每天=86400、每週=604800，不重複則填 0）。步驟3：呼叫 schedule_task 完成排程。步驟4：回覆使用者已排程的時間與任務內容。",
        tools: &["get_current_datetime", "schedule_task"],
        need_tool_chain: true,
        tool_chain_order: &["get_current_datetime", "schedule_task"],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_save_knowledge",
        title: "儲存知識/洞見到知識庫",
        trigger: "儲存這個知識、把這個記下來、存成知識、記錄這個洞見、把這段存進知識庫、幫我歸納存檔、這個值得記錄、存到knowledge、儲存這次的結論、把這個整理成知識卡、save knowledge、compress to knowledge、知識壓縮、歸納成知識、把這個存起來、幫我保存這個見解",
        behavior: "步驟1：分析對話內容，歸納核心知識點（若使用者有指定內容則直接使用）。步驟2：確定標題（簡潔，不超過 30 字）、內容（結構化 Markdown）、標籤（2-4 個相關主題詞）。步驟3：呼叫 compress_to_knowledge（title、content、tags）將知識存入 knowledge/ 資料夾。步驟4：告知使用者已儲存的路徑，並簡述儲存的主要知識點。",
        tools: &["compress_to_knowledge", "find_similar_notes"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_deep_research",
        title: "深度研究整合（多篇筆記）",
        trigger: "深度研究、整合多篇筆記、綜合分析、跨筆記整理、找出關聯、知識整合、comprehensive research、deep dive、幫我深入研究、把相關筆記都找出來整合、綜合所有相關資料、跨文件分析、全面整理、多篇整合摘要、相關筆記分析",
        behavior: "步驟1：呼叫 search_vault（query 填研究主題）找出相關筆記清單。步驟2：呼叫 find_similar_notes（query 填研究主題，limit 填 8）補充向量語意相近的筆記。步驟3：合併兩個清單，去除重複，選出最相關的 5-8 篇。步驟4：呼叫 summarize_note_collection（paths 填選出的路徑陣列，query 填研究重點）生成整合摘要。步驟5：根據摘要回答使用者，並列出來源筆記路徑。",
        tools: &["search_vault", "find_similar_notes", "summarize_note_collection", "read_note"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_personal_insight",
        title: "個人洞察與知識庫分析",
        trigger: "分析我的知識庫、了解我的習慣、知識庫健康度、我的學習模式、分析我的筆記習慣、你了解我的偏好嗎、知識庫概況、vault analytics、analyze my vault、我有多少筆記、知識庫統計、個人化建議、了解我的學習習慣、幫我分析一下知識庫、我的知識圖譜",
        behavior: "步驟1：呼叫 get_vault_stats 取得知識庫整體統計（筆記數、資料夾數、字數、最近修改）。步驟2：呼叫 distill_preferences 分析使用者偏好（從對話記憶萃取）。步驟3：根據統計與偏好，提供個人化洞察：（a）知識庫概況評估；（b）可能的盲點或未覆蓋領域；（c）改善建議。步驟4：若使用者想了解特定筆記的關聯，呼叫 get_note_backlinks 分析反向連結。",
        tools: &["get_vault_stats", "distill_preferences", "get_note_backlinks", "find_similar_notes"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_update_metadata",
        title: "更新筆記屬性/標籤",
        trigger: "更新標籤、修改屬性、加上標籤、改狀態、標記已完成、更新 frontmatter、add tag、update status、mark as done、幫我加個標籤、把這篇標記為、更新筆記的 tags、改一下 status、幫我更新屬性、修改 metadata、設定優先級",
        behavior: "步驟1：若路徑不確定，呼叫 search_vault 取得目標筆記的精確路徑。步驟2：確認要更新的欄位（如 tags、status、priority、due_date 等）與新值。步驟3：呼叫 update_note_frontmatter（path 填精確路徑，fields 填要更新的鍵值對）。步驟4：回覆使用者已更新的欄位與新值。",
        tools: &["search_vault", "update_note_frontmatter"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_knowledge_graph",
        title: "探索知識圖譜關聯",
        trigger: "哪些筆記連到這篇、反向連結、知識圖譜、找出關聯筆記、這篇被哪些筆記引用、backlinks、linked mentions、who links here、知識網絡、找出所有引用、筆記之間的關係、知識關聯圖、探索連結",
        behavior: "步驟1：若路徑不確定，呼叫 search_vault 取得目標筆記的精確路徑。步驟2：呼叫 get_note_backlinks（path 填目標路徑）取得所有反向連結。步驟3：呼叫 extract_note_links（path 填目標路徑）取得出向連結。步驟4：呼叫 find_similar_notes（query 填目標筆記標題或主題）補充語意相關但未明確連結的筆記。步驟5：整合回覆反向連結、出向連結、語意相關筆記清單。",
        tools: &["search_vault", "get_note_backlinks", "extract_note_links", "find_similar_notes", "link_notes"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_tag_browse",
        title: "以標籤瀏覽筆記",
        trigger: "標籤是X的筆記、有tag X的筆記、找tag、按標籤查、search by tag、filter by tag、給我標籤X的、tag filter、以標籤篩選、顯示所有X標籤、標籤搜尋、有哪些筆記有這個標籤、找出有X tag的",
        behavior: "步驟1：從使用者訊息中提取標籤名稱。步驟2：呼叫 search_by_tag（tag 填標籤名稱）取得符合的筆記列表。步驟3：回覆找到的筆記清單。步驟4（可選）：若使用者想看特定筆記的內容，呼叫 read_note 或 open_note。",
        tools: &["search_by_tag", "read_note", "open_note"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_task_extraction",
        title: "提取待辦事項/任務清單",
        trigger: "有什麼待辦、幫我整理TODO、列出所有待辦、找出action item、這資料夾有哪些任務、extract tasks、list todos、找出所有TODO、整理一下代辦事項、有哪些未完成的、把待辦列出來、任務清單、action items、check todos、pending tasks",
        behavior: "步驟1：確認掃描範圍（單一筆記或整個資料夾）。若使用者指定筆記 → path 參數；若指定資料夾 → folder 參數；若未指定 → 先呼叫 list_structure 讓使用者選擇。步驟2：呼叫 extract_action_items（填入 path 或 folder）取得所有待辦事項。步驟3：以清單格式回覆，標示來源筆記。步驟4（可選）：若使用者想排程某個待辦，呼叫 get_current_datetime + schedule_task 完成排程。",
        tools: &["extract_action_items", "list_structure", "get_current_datetime", "schedule_task"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_vault_health",
        title: "知識庫健康診斷",
        trigger: "知識庫健康、診斷筆記庫、找孤立筆記、哪些筆記沒有連結、vault health、orphan notes、孤立筆記、知識庫問題、找出未連接的、沒被引用的筆記、孤兒筆記、診斷一下我的知識庫、幫我找出問題、知識庫整理診斷",
        behavior: "步驟1：呼叫 get_vault_stats 取得知識庫概況。步驟2：呼叫 find_orphan_notes 找出所有沒有反向連結的孤立筆記。步驟3：整合報告：（a）知識庫概況；（b）孤立筆記清單；（c）建議行動（用 find_similar_notes 為每篇孤立筆記找相關筆記，再用 link_notes 建立連結）。步驟4（可選）：若使用者同意修復，對孤立筆記逐一呼叫 find_similar_notes，找到相關筆記後呼叫 link_notes 建立連結。",
        tools: &["get_vault_stats", "find_orphan_notes", "find_similar_notes", "link_notes"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_generate_index",
        title: "生成資料夾目錄筆記（MOC）",
        trigger: "幫我生成目錄、建立索引筆記、generate MOC、create index、幫我做一個索引、生成 Map of Contents、這個資料夾的目錄、建立目錄頁、資料夾索引、自動生成目錄、建立 MOC 筆記、index note、幫我整理一個目錄",
        behavior: "步驟1：確認目標資料夾路徑（若使用者未指定，呼叫 list_structure 列出可選資料夾）。步驟2（可選）：詢問使用者是否要自訂輸出路徑，或使用預設的 {folder}/index.md。步驟3：呼叫 generate_moc（folder 填資料夾路徑，output_path 可選）生成 MOC 筆記。步驟4：告知使用者 MOC 已生成在哪個路徑，並簡述包含幾篇筆記。",
        tools: &["list_structure", "generate_moc"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_recent_activity",
        title: "查看最近筆記活動",
        trigger: "最近寫了什麼、這週修改了什麼、最近的筆記、recent notes、recently modified、最近幾天的筆記、最近活動、看看最近在做什麼、這幾天有什麼更新、最近有哪些變動、recent activity、我最近在研究什麼、昨天改了什麼",
        behavior: "步驟1：呼叫 list_recent_notes（days 根據使用者說的時間範圍填入，預設 7；使用者說「昨天」填 1、「這週」填 7、「這個月」填 30）取得最近修改的筆記。步驟2：以清單格式回覆，包含筆記標題、路徑、修改時間、字數。步驟3（可選）：若使用者想深入了解某篇，呼叫 open_note 或 read_note。",
        tools: &["list_recent_notes", "open_note", "read_note"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_link_builder",
        title: "建立筆記間連結",
        trigger: "幫我把這兩篇筆記連起來、加一個連結到另一篇、在這篇筆記加入wiki連結、link two notes、create link、建立知識連結、把這篇連到那篇、在筆記裡加入參考連結、幫我建立連結、把A連到B、加上相關筆記連結、建立交叉連結",
        behavior: "步驟1：確認 from_path（要加連結的筆記）和 to_path（要被連結的目標筆記）。若路徑不確定，分別呼叫 search_vault 取得精確路徑。步驟2（可選）：呼叫 extract_note_links（path 填 from_path）確認是否已有連結，避免重複。步驟3：呼叫 link_notes（from_path、to_path、section 可選）插入 [[wiki link]]。步驟4：回覆已建立連結的確認，並說明連結插入在哪個章節。",
        tools: &["search_vault", "extract_note_links", "link_notes"],
        need_tool_chain: false,
        tool_chain_order: &[],
        injection_mode: "passive",
    },
    BuiltinSkill {
        id: "builtin_proactive_memory",
        title: "主動記憶注入",
        trigger: "__proactive__",
        behavior: "在每次對話開始時，自動根據使用者訊息的主題從記憶庫中擷取相關事實，靜默注入為背景知識，無需使用者主動詢問。",
        tools: &["prefetch_memory"],
        need_tool_chain: true,
        tool_chain_order: &["prefetch_memory"],
        injection_mode: "proactive",
    },
];

const AGENTS: &[BuiltinAgent] = &[
    BuiltinAgent {
        id: "builtin_skill_builder",
        name: "技能建立助理",
        description: "根據知識描述自動設計並建立 Agent 技能規範",
        kind: "sub",
        tool_names: &["create_agent_skill"],
        system_prompt: "你是技能建立助理，專門根據描述設計 Agent 技能規範。\n\
            ## 任務\n\
            根據使用者提供的知識描述或需求，呼叫 create_agent_skill 設計 1-2 個技能規範。\n\
            每個技能必須有清楚的觸發情境（trigger）、分步驟的行為描述（behavior）、以及從系統工具清單中選擇真正需要的工具（tool_calls）。\n\
            只呼叫 create_agent_skill 工具，不要輸出其他文字。",
        max_rounds: 3,
        trigger: "建立技能規範、新增技能、幫我設計skill、創建技能、新建skill、design skill、create skill spec",
    },
    BuiltinAgent {
        id: "builtin_scheduler",
        name: "排程助理",
        description: "幫使用者設定任務提醒、定時通知與週期性排程",
        kind: "sub",
        tool_names: &["get_current_datetime", "schedule_task"],
        system_prompt: "你是排程助理，專門幫使用者設定任務提醒與排程。\n\
            ## 工作流程\n\
            1. 先呼叫 get_current_datetime 確認現在的時間與時區。\n\
            2. 根據使用者描述計算執行時間 run_at（ISO 8601 含時區，如 2026-03-22T09:00:00+08:00）。\n\
            3. 若需重複，填 repeat_interval_seconds（每天=86400、每週=604800、每月≈2592000，不重複填 0）。\n\
            4. 呼叫 schedule_task（description、run_at、repeat_interval_seconds）完成排程。\n\
            5. 用友善語氣確認排程結果（告知使用者會在何時收到提醒）。\n\
            ## 注意\n\
            - 時間若使用者只說「明天」、「三點」等相對語，需結合步驟1的現在時間計算。\n\
            - 永遠確保 run_at 是未來時間。",
        max_rounds: 3,
        trigger: "排程、設定提醒、定時任務、設定鬧鐘、幫我排程、提醒我、到時候提醒、schedule、remind me、set reminder、每天提醒、每週提醒、固定時間、重複執行",
    },
    BuiltinAgent {
        id: "builtin_note_card_advisor",
        name: "筆記卡片助理",
        description: "根據知識內容分析並建立 concept/procedure/reference 型筆記卡片",
        kind: "sub",
        tool_names: &["search_vault", "read_note", "create_note", "plan_announce"],
        system_prompt: "你是筆記卡片助理，專門根據知識內容生成結構化的筆記卡片。\n\
            ## 卡片模板類型\n\
            - **concept**：概念定義卡，包含定義、詳細說明、範例\n\
            - **procedure**：操作步驟卡，包含前提條件、步驟清單、注意事項\n\
            - **reference**：參考資料卡，包含摘要、重要連結、關鍵點清單\n\
            ## 工作流程\n\
            1. 若使用者指定筆記，呼叫 search_vault 或 read_note 取得原始內容。\n\
            2. 分析內容，決定適合哪些模板類型（通常 2-3 張）。\n\
            3. 呼叫 plan_announce 列出將建立的卡片清單，deferred_tools 填 create_note。\n\
            4. 使用者確認後，逐一呼叫 create_note（路徑格式：cards/[標題].md）。\n\
            5. 回覆使用者已建立的卡片列表。",
        max_rounds: 5,
        trigger: "建立筆記卡片、知識卡片、建議卡片、整理成卡片、幫我做成卡片、suggest note card、create note card、concept 卡片、procedure 卡片、reference 卡片",
    },
    BuiltinAgent {
        id: "builtin_note_summarizer",
        name: "筆記整理助理",
        description: "閱讀多篇筆記並產出摘要、彙整或結構化整理",
        kind: "sub",
        tool_names: &[
            "search_vault", "list_notes_in_folder", "list_structure",
            "read_note", "create_note", "update_note", "plan_announce",
        ],
        system_prompt: "你是筆記整理助理，專門閱讀多篇筆記並產出摘要或結構化整理。\n\
            ## 工作流程\n\
            1. 用 search_vault 或 list_notes_in_folder 取得相關筆記清單。\n\
            2. 對清單中每篇筆記呼叫 read_note 讀取完整內容。\n\
            3. 彙整後輸出結構化摘要（分節標題、要點、結論）。\n\
            4. 若使用者希望儲存整理成果，呼叫 plan_announce 說明計畫，使用者確認後再 create_note 或 update_note 寫入。\n\
            ## 注意\n\
            - 摘要要忠實反映筆記原意，不要自行發明內容。\n\
            - 若筆記數量超過 10 篇，先列出清單讓使用者確認範圍再逐一閱讀。",
        max_rounds: 8,
        trigger: "整理筆記、摘要多篇、幫我歸納、彙整資料、總結所有、整合多篇、summarize notes、consolidate、把這些筆記整理、歸納重點、統整一下、彙整成一篇、做個摘要、整理一份報告",
    },
];

/// 為指定 account 幂等重建所有內建 skills 與 agent definitions。
/// 若此 account 已有 builtin 資料則直接回傳（幂等）。
/// 強制重建請傳 force = true（目前預留，呼叫端永遠傳 false）。
pub async fn seed_builtins(db: &SurrealDb, account_id: &str) {
    // 幂等判斷：已有 builtin skill 則跳過
    #[derive(serde::Deserialize)]
    struct CountRow { count: i64 }
    if let Ok(mut r) = db.query(
        "SELECT count() AS count FROM agent_skills WHERE account_id = $aid AND knowledge_item_id = '__builtin__' GROUP ALL"
    ).bind(("aid", account_id.to_string())).await {
        let rows: Vec<CountRow> = r.take(0).unwrap_or_default();
        if rows.into_iter().next().map(|r| r.count).unwrap_or(0) > 0 {
            tracing::debug!("seed_builtins: account {} already seeded, skipping", account_id);
            return;
        }
    }

    let now = Utc::now().timestamp();

    // ── Skills ───────────────────────────────────────────────────────────────
    let _ = db.query(
        "DELETE agent_skills WHERE account_id = $aid AND knowledge_item_id = '__builtin__'"
    )
    .bind(("aid", account_id.to_string()))
    .await;

    for s in SKILLS {
        let tools_json: serde_json::Value = serde_json::Value::Array(
            s.tools.iter().map(|t| serde_json::Value::String(t.to_string())).collect(),
        );
        let chain_order_json: serde_json::Value = serde_json::Value::Array(
            s.tool_chain_order.iter().map(|t| serde_json::Value::String(t.to_string())).collect(),
        );
        let _ = db.query(
            "INSERT INTO agent_skills \
             (skill_id, account_id, knowledge_item_id, title, trigger, behavior, \
              tool_calls, is_active, injection_mode, agent_scope, \
              need_tool_chain, tool_chain_order, trigger_count, created_at) \
             VALUES ($sid, $aid, '__builtin__', $title, $trigger, $behavior, \
                     $tools, true, $imode, 'all', \
                     $need_chain, $chain_order, 0, $now)"
        )
        .bind(("sid",         s.id.to_string()))
        .bind(("aid",         account_id.to_string()))
        .bind(("title",       s.title.to_string()))
        .bind(("trigger",     s.trigger.to_string()))
        .bind(("behavior",    s.behavior.to_string()))
        .bind(("tools",       tools_json))
        .bind(("imode",       s.injection_mode.to_string()))
        .bind(("need_chain",  s.need_tool_chain))
        .bind(("chain_order", chain_order_json))
        .bind(("now",         now))
        .await;
    }

    tracing::info!("Seeded {} builtin skills for account {}", SKILLS.len(), account_id);

    // ── Agent Definitions ────────────────────────────────────────────────────
    let _ = db.query(
        "DELETE agent_definitions WHERE account_id = $aid AND is_builtin = true"
    )
    .bind(("aid", account_id.to_string()))
    .await;

    for a in AGENTS {
        let tool_names_json: serde_json::Value = serde_json::Value::Array(
            a.tool_names.iter().map(|t| serde_json::Value::String(t.to_string())).collect(),
        );
        let _ = db.query(
            "INSERT INTO agent_definitions \
             (def_id, account_id, name, description, kind, skill_ids, tool_names, \
              system_prompt, max_rounds, is_active, is_builtin, trigger, \
              status, use_count, created_at) \
             VALUES ($did, $aid, $name, $desc, $kind, [], $tools, \
                     $prompt, $rounds, true, true, $trigger, \
                     'active', 0, $now)"
        )
        .bind(("did",    a.id.to_string()))
        .bind(("aid",    account_id.to_string()))
        .bind(("name",   a.name.to_string()))
        .bind(("desc",   a.description.to_string()))
        .bind(("kind",   a.kind.to_string()))
        .bind(("tools",  tool_names_json))
        .bind(("prompt", a.system_prompt.to_string()))
        .bind(("rounds", a.max_rounds))
        .bind(("trigger",a.trigger.to_string()))
        .bind(("now",    now))
        .await;
    }

    tracing::info!("Seeded {} builtin agents for account {}", AGENTS.len(), account_id);
}
