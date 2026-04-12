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
    /// Behavior text with @[tool_name] markers defining the chain execution order.
    behavior: &'static str,
    injection_mode: &'static str,
    /// Bump to force-update this skill for all existing users on next startup.
    /// Increment manually in code when behavior/trigger/title changes in a release.
    seed_version: u32,
}

struct BuiltinAgent {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    kind: &'static str,
    tool_names: &'static [&'static str],
    use_skill_pass: bool,
    enable_think: bool,
    system_prompt: &'static str,
    max_rounds: i64,
    trigger: &'static str,
}

const SKILLS: &[BuiltinSkill] = &[
    BuiltinSkill {
        id: "builtin_open_note",
        title: "打開/查看筆記",
        trigger: "打開、開啟、open、打開筆記、開啟文件、跳轉到筆記、查看筆記、幫我看某篇、看一下某個、切換到筆記、show me the note、open note、display note、navigate to、go to note、讓我看看、帶我去、開那篇",
        behavior: "步驟1：呼叫 @[search_vault]，傳入使用者提到的筆記名稱作為 query（若包含 .md 副檔名請去除），取得精確的檔案路徑。步驟2：用步驟1取得的路徑呼叫 @[open_note]。步驟3：只回覆「已打開 [筆記名稱]」，絕對不要輸出筆記內容。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_edit_note",
        title: "編輯/修改筆記",
        trigger: "修改、編輯、更新、改、修改筆記、編輯文件、更新內容、改一下某篇、幫我改、修改某段、重寫某節、edit note、update note、revise、幫我更新、把這段改成、把內容換成、調整一下、改掉、替換內容、覆寫",
        behavior: "步驟1：呼叫 @[search_vault] 取得目標筆記的精確路徑。步驟2：呼叫 @[read_note] 讀取完整現有內容。步驟3：呼叫 @[plan_announce] 告知使用者將修改哪個檔案、改什麼內容。步驟4：使用者確認後，將修改後的完整內容呼叫 @[update_note] 寫入。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_create_note",
        title: "新增/建立筆記",
        trigger: "建立、新增、寫、記、建立新筆記、寫一篇、新增文件、記錄下來、幫我新增、建立一份、寫個筆記、create note、new note、write a note、幫我寫、新建一個、記一下、存成筆記、建立文件、新增一篇",
        behavior: "步驟1：若使用者未指定資料夾，先呼叫 @[list_structure]（path 傳空字串）查看現有目錄結構，選擇合適的存放位置。步驟2：呼叫 @[plan_announce]，告知使用者將建立的檔案路徑與內容摘要。步驟3：使用者確認後，呼叫 @[create_note]（path 格式為 folder/filename.md，content 為完整 Markdown 內容）。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_append_note",
        title: "追加/補充筆記內容",
        trigger: "在筆記末尾加、補充內容、追加到筆記、把這個加進去、加到筆記裡、繼續寫到某篇、append to note、add to note、在後面加上、補上去、加一段、繼續記錄、把這段加進",
        behavior: "步驟1：呼叫 @[search_vault] 取得目標筆記的精確路徑。步驟2：呼叫 @[plan_announce]，說明要追加的內容。步驟3：使用者確認後，呼叫 @[append_to_note]（path 使用步驟1的路徑，content 為要追加的文字，會自動加在末尾不覆蓋原有內容）。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_search_note",
        title: "搜尋/查詢筆記",
        trigger: "搜尋、找、查、搜尋筆記、找資料、查某個主題、有沒有寫過、幫我找、找找看、查詢相關筆記、search notes、find note、look up、我有沒有寫、我有記過嗎、筆記裡有沒有、幫我查一下、找找有沒有、知識庫裡有",
        behavior: "步驟1：判斷搜尋類型——若使用者問的是語意/主題（如「有什麼關於 X 的筆記」），呼叫 @[search_vault]；若使用者要找特定文字/程式碼/精確詞組（如「哪裡有寫到 function foo」），呼叫 @[grep_vault]。步驟2：根據搜尋結果回覆使用者找到哪些筆記。步驟3（可選）：若使用者需要看完整內容，用搜尋結果中的路徑呼叫 read_note；若使用者要打開筆記，呼叫 open_note。",
        injection_mode: "passive",
        seed_version: 2,
    },
    BuiltinSkill {
        id: "builtin_summarize_notes",
        title: "整理/摘要多篇筆記",
        trigger: "整理筆記、摘要多篇、幫我歸納、整合資料、總結所有、彙整、整理一份、summarize notes、consolidate、幫我做個總結、把這些筆記整理、歸納重點、統整一下、彙整成一篇、做個摘要",
        behavior: "步驟1：呼叫 @[search_vault] 取得相關筆記清單（取得 path 列表）。步驟2：呼叫 @[batch_read_notes]（paths 填步驟1的路徑陣列，一次讀取所有筆記內容）。步驟3：彙整所有內容後輸出摘要。步驟4（可選）：若使用者要存成新筆記，呼叫 @[plan_announce] 確認後再 @[create_note] 建立。",
        injection_mode: "passive",
        seed_version: 2,
    },
    BuiltinSkill {
        id: "builtin_browse_structure",
        title: "瀏覽資料夾結構",
        trigger: "看資料夾、瀏覽結構、有什麼筆記、列出所有、Vault 裡有什麼、資料夾有哪些、目錄結構、list folders、show structure、browse vault、我有哪些資料夾、筆記庫裡有什麼、幫我看一下有什麼檔案、目前有哪些筆記",
        behavior: "步驟1：呼叫 @[list_structure]（path 傳空字串表示根目錄）取得整個 Vault 的資料夾與檔案樹狀結構。步驟2（可選）：若使用者要看特定資料夾的筆記列表，呼叫 list_notes_in_folder。步驟3：以清單格式回覆結構內容。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_delete_note",
        title: "刪除筆記",
        trigger: "刪除筆記、移除文件、把某篇刪掉、刪掉這個筆記、清除某個文件、delete note、remove note、trash note、把這篇刪了、幫我刪、不要那篇了、刪除這份文件、清掉",
        behavior: "步驟1：呼叫 @[search_vault] 確認要刪除的筆記精確路徑。步驟2：呼叫 @[plan_announce]，明確告知使用者「將永久刪除 [路徑]，此操作不可復原」。步驟3：等使用者明確確認後才呼叫 @[delete_note]。若使用者取消則不執行。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_move_note",
        title: "移動/重新命名筆記",
        trigger: "移動筆記、重新命名、換個位置、改檔名、搬移、把筆記移到、重命名、move note、rename note、relocate、把這篇搬到、改個名字、換個名稱、移到另一個資料夾、把文件移過去",
        behavior: "步驟1：呼叫 @[search_vault] 確認來源筆記的精確路徑（from 參數）。步驟2：呼叫 @[plan_announce] 說明搬移計畫，並請使用者確認目標路徑（to 參數，格式如 new_folder/new_name.md）。步驟3：使用者確認後呼叫 @[move_note]。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_organize_folders",
        title: "整理資料夾架構",
        trigger: "建立資料夾、整理目錄、新增分類、重新整理資料夾、建立分類結構、規劃目錄、create folder、organize folders、新增一個資料夾、幫我建個目錄、整理一下分類、建立子目錄、新增分類夾",
        behavior: "步驟1：呼叫 @[list_structure]（path 傳空字串）了解現有資料夾結構。步驟2：根據使用者需求規劃新架構，呼叫 @[plan_announce] 說明整體計畫。步驟3：使用者確認後，依序呼叫 @[create_folder] 建立新資料夾。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_query_memory",
        title: "查詢過去對話記憶",
        trigger: "之前說過什麼、記憶查詢、歷史對話、上次討論、我之前提到、你記得嗎、過去的對話、recall memory、what did I say、上次我們聊、你還記得、之前提過、我跟你說過、先前的對話、記得我說的",
        behavior: "步驟1：從使用者問題中提取關鍵字（人名、主題、事件等）。步驟2：呼叫 @[query_memory]（keywords 填提取的關鍵字陣列，若無關鍵字則傳空陣列取最新記憶）。步驟3：根據回傳的記憶內容回答使用者。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_web_search",
        title: "網路搜尋/外部資訊",
        trigger: "搜尋網路、查最新資訊、Google 一下、查新聞、找外部資料、搜網路、最新消息、web search、search the web、look it up online、查天氣、今天天氣、現在幾度、股價、最新匯率、即時資訊、查一下網路、幫我搜尋、去網路上找、外部資訊、最新動態、新聞、時事、最近發生什麼",
        behavior: "步驟1：呼叫 @[web_search]（query 填具體搜尋關鍵字），取得最新網路資訊與結果連結。步驟2（可選）：若搜尋結果摘要不夠完整、或使用者需要完整內容，從結果中選取最相關的 URL，呼叫 @[fetch_url] 取得頁面完整文字內容。步驟3：根據收集到的資訊回答使用者。",
        injection_mode: "passive",
        seed_version: 2,
    },
    BuiltinSkill {
        id: "builtin_create_skill",
        title: "新增/建立技能規範",
        trigger: "新增技能、建立技能、設計技能規範、幫我建立skill、新增skill、新建技能、create skill、design skill、建一個技能、幫我設計一個skill、新增一個agent技能、我要建立技能、建立技能規範、新增行為規則",
        behavior: "步驟1：請使用者描述技能的目的（觸發情境、希望AI執行什麼操作）。步驟2：根據描述呼叫 @[create_agent_skill]：title（技能標題）、trigger（觸發語境）、behavior（分步驟操作說明，用 @[tool_name] 標記工具）、injection_mode（'passive'）。步驟3：告知使用者技能已建立，說明什麼情況下會被觸發。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_suggest_note_cards",
        title: "AI 建議筆記卡片",
        trigger: "幫我建立筆記卡片、建立知識卡片、整理成卡片、幫我產生筆記卡片、建議筆記卡片、生成卡片、suggest note cards、create note card、知識卡片建議、幫我做成卡片、把這個整理成卡片、幫我萃取卡片、建立 concept 卡片、建立 procedure 卡片、建立 reference 卡片",
        behavior: "步驟1：呼叫 @[search_vault] 或 @[read_note] 取得使用者指定的知識內容。步驟2：分析內容，依 concept、procedure、reference 三種模板各生成 1 張筆記卡片建議。步驟3：呼叫 @[plan_announce] 列出將建立的卡片清單。步驟4：使用者確認後，逐一呼叫 @[create_note]（路徑格式：cards/[標題].md）。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_schedule_task",
        title: "排程/提醒任務",
        trigger: "排程、設定提醒、定時任務、設定鬧鐘、幫我排程、提醒我、到時候提醒、設定時間、定時提醒、schedule、remind me、set reminder、set alarm、幫我設一個提醒、X點提醒我、明天提醒、每天提醒、固定時間、重複執行、定期通知、設定排程任務、每週提醒",
        behavior: "步驟1：呼叫 @[get_current_datetime] 取得現在時間（作為計算相對時間的基準）。步驟2：根據使用者描述確定 description、run_at（ISO 8601 含時區）、repeat_interval_seconds（秒數，不重複填 0）、agent_def_name（可選，如 'memory_agent'）。步驟3：呼叫 @[schedule_task] 完成排程。步驟4：回覆使用者已排程的時間與任務內容。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_save_knowledge",
        title: "儲存知識/洞見到知識庫",
        trigger: "儲存這個知識、把這個記下來、存成知識、記錄這個洞見、把這段存進知識庫、幫我歸納存檔、這個值得記錄、存到knowledge、儲存這次的結論、把這個整理成知識卡、save knowledge、compress to knowledge、知識壓縮、歸納成知識、把這個存起來、幫我保存這個見解",
        behavior: "步驟1：分析對話內容，歸納核心知識點。步驟2：確定標題（不超過 30 字）、內容（結構化 Markdown）、標籤（2-4 個主題詞）。步驟3：呼叫 @[compress_to_knowledge]（title、content、tags）將知識存入 knowledge/ 資料夾。步驟4：告知使用者已儲存的路徑。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_deep_research",
        title: "深度研究整合（多篇筆記）",
        trigger: "深度研究、整合多篇筆記、綜合分析、跨筆記整理、找出關聯、知識整合、comprehensive research、deep dive、幫我深入研究、把相關筆記都找出來整合、綜合所有相關資料、跨文件分析、全面整理、多篇整合摘要、相關筆記分析",
        behavior: "步驟1：呼叫 @[search_vault]（query 填研究主題）找出相關筆記清單。步驟1b（可選）：若需要找特定詞彙或程式碼出現位置，同時呼叫 @[grep_vault]（pattern 填精確關鍵字）補充語意搜尋的不足。步驟2：呼叫 @[batch_read_notes]（paths 填步驟1/1b 取得的路徑陣列，一次讀取全部）取得完整內容。步驟3：彙整後輸出整合摘要，並列出來源筆記路徑。",
        injection_mode: "passive",
        seed_version: 2,
    },
    BuiltinSkill {
        id: "builtin_personal_insight",
        title: "個人洞察與知識庫分析",
        trigger: "分析我的知識庫、了解我的習慣、知識庫健康度、我的學習模式、分析我的筆記習慣、你了解我的偏好嗎、知識庫概況、vault analytics、analyze my vault、我有多少筆記、知識庫統計、個人化建議、了解我的學習習慣、幫我分析一下知識庫、我的知識圖譜",
        behavior: "步驟1：呼叫 @[get_vault_stats] 取得知識庫整體統計。步驟2：根據統計提供個人化洞察：知識庫概況、可能的盲點、改善建議。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_update_metadata",
        title: "更新筆記屬性/標籤",
        trigger: "更新標籤、修改屬性、加上標籤、改狀態、標記已完成、更新 frontmatter、add tag、update status、mark as done、幫我加個標籤、把這篇標記為、更新筆記的 tags、改一下 status、幫我更新屬性、修改 metadata、設定優先級",
        behavior: "步驟1：呼叫 @[search_vault] 取得目標筆記的精確路徑。步驟2：確認要更新的欄位（如 tags、status、priority、due_date 等）與新值。步驟3：呼叫 @[update_note_frontmatter]（path 填精確路徑，fields 填要更新的鍵值對）。步驟4：回覆使用者已更新的欄位與新值。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_knowledge_graph",
        title: "探索知識圖譜關聯",
        trigger: "哪些筆記連到這篇、反向連結、知識圖譜、找出關聯筆記、這篇被哪些筆記引用、backlinks、linked mentions、who links here、知識網絡、找出所有引用、筆記之間的關係、知識關聯圖、探索連結",
        behavior: "步驟1：呼叫 @[search_vault] 取得目標筆記的精確路徑。步驟2：呼叫 @[get_note_backlinks]（path 填目標路徑）取得所有反向連結。步驟3：整合回覆反向連結與語意相關筆記清單。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_tag_browse",
        title: "以標籤瀏覽筆記",
        trigger: "標籤是X的筆記、有tag X的筆記、找tag、按標籤查、search by tag、filter by tag、給我標籤X的、tag filter、以標籤篩選、顯示所有X標籤、標籤搜尋、有哪些筆記有這個標籤、找出有X tag的",
        behavior: "步驟1：從使用者訊息中提取標籤名稱。步驟2：呼叫 @[search_by_tag]（tag 填標籤名稱）取得符合的筆記列表。步驟3：回覆找到的筆記清單。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_task_extraction",
        title: "提取待辦事項/任務清單",
        trigger: "有什麼待辦、幫我整理TODO、列出所有待辦、找出action item、這資料夾有哪些任務、extract tasks、list todos、找出所有TODO、整理一下代辦事項、有哪些未完成的、把待辦列出來、任務清單、action items、check todos、pending tasks",
        behavior: "步驟1：確認掃描範圍。若使用者未指定先呼叫 @[list_structure] 讓使用者選擇。步驟2：呼叫 @[extract_action_items]（填入 path 或 folder）取得所有待辦事項。步驟3：以清單格式回覆，標示來源筆記。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_vault_health",
        title: "知識庫健康診斷",
        trigger: "知識庫健康、診斷筆記庫、找孤立筆記、哪些筆記沒有連結、vault health、orphan notes、孤立筆記、知識庫問題、找出未連接的、沒被引用的筆記、孤兒筆記、診斷一下我的知識庫、幫我找出問題、知識庫整理診斷",
        behavior: "步驟1：呼叫 @[get_vault_stats] 取得知識庫概況。步驟2：呼叫 @[find_orphan_notes] 找出所有沒有反向連結的孤立筆記。步驟3：整合報告知識庫概況、孤立筆記清單、改善建議。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_generate_index",
        title: "生成資料夾目錄筆記（MOC）",
        trigger: "幫我生成目錄、建立索引筆記、generate MOC、create index、幫我做一個索引、生成 Map of Contents、這個資料夾的目錄、建立目錄頁、資料夾索引、自動生成目錄、建立 MOC 筆記、index note、幫我整理一個目錄",
        behavior: "步驟1：確認目標資料夾路徑（若使用者未指定，呼叫 @[list_structure] 列出可選資料夾）。步驟2：呼叫 @[generate_moc]（folder 填資料夾路徑）生成 MOC 筆記。步驟3：告知使用者 MOC 已生成在哪個路徑。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_recent_activity",
        title: "查看最近筆記活動",
        trigger: "最近寫了什麼、這週修改了什麼、最近的筆記、recent notes、recently modified、最近幾天的筆記、最近活動、看看最近在做什麼、這幾天有什麼更新、最近有哪些變動、recent activity、我最近在研究什麼、昨天改了什麼",
        behavior: "步驟1：呼叫 @[list_recent_notes]（days 根據使用者說的時間範圍填入，預設 7）取得最近修改的筆記。步驟2：以清單格式回覆，包含筆記標題、路徑、修改時間、字數。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_link_builder",
        title: "建立筆記間連結",
        trigger: "幫我把這兩篇筆記連起來、加一個連結到另一篇、在這篇筆記加入wiki連結、link two notes、create link、建立知識連結、把這篇連到那篇、在筆記裡加入參考連結、幫我建立連結、把A連到B、加上相關筆記連結、建立交叉連結",
        behavior: "步驟1：呼叫 @[search_vault] 確認來源筆記（from_path）的精確路徑。步驟2：呼叫 @[link_notes]（from_path、to_path、section 可選）插入 [[wiki link]]。步驟3：回覆已建立連結的確認。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_proactive_memory",
        title: "主動記憶注入",
        trigger: "__proactive__",
        behavior: "在每次對話開始時，自動根據使用者訊息的主題從記憶庫中擷取相關事實，靜默注入為背景知識，無需使用者主動詢問。@[prefetch_memory]",
        injection_mode: "proactive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_kb_search",
        title: "知識庫問答",
        trigger: "__kb_query__",
        behavior: "步驟1：呼叫 @[search_kb_pages]（query 填使用者問題，session_id 若有則帶入）搜尋知識庫已匯入頁面，取得含 __cite_id__ 的結果。步驟2：根據結果回答使用者問題。回答第一句必須以 [cite:id1,id2] 格式標明所引用的結果 cite_id；若結果為空，回覆「目前沒有相關的知識點」。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_organize_memory",
        title: "整理/提取記憶",
        trigger: "整理記憶、提取記憶、分析對話記憶、幫我整理記憶、記憶整理、萃取記憶、從對話提取知識、記憶分析、幫我歸納記憶、organize memory、extract memory、memory cleanup、分析我的對話記憶、整理過去的對話、把對話轉成記憶、記憶萃取",
        behavior: "步驟1：呼叫 get_unprocessed_conversations 取得尚未分析記憶的對話列表。步驟2：對每個對話呼叫 get_conversation_content 取得完整訊息內容。步驟3：判斷是否有長期記憶價值，有 → 呼叫 save_memory_facts；無論如何每個對話都呼叫 mark_conversation_processed。步驟4：全部完成後呼叫 condense_memory_facts。步驟5：回覆整理摘要。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_calendar",
        title: "Google Calendar 行程查詢/建立",
        trigger: "我今天有什麼行程、查行程、看行程、今天的行程、明天行程、這週有什麼、幫我加個行程、建立行程、新增行程、安排會議、calendar、schedule event、check calendar、what's on my calendar、幫我排一個、加到行事曆、建一個會議、設定行程",
        behavior: "步驟1：若使用者要查詢行程，呼叫 @[get_calendar_events]（date 填使用者指定日期或今天，days 填查詢天數）。步驟2：若使用者要建立行程，先確認標題、時間（從【當前時間】推算，含時區），再呼叫 @[create_calendar_event]（title、start、end 使用 RFC 3339 格式含時區）。若 Google Calendar 尚未連接，告知使用者前往設定頁面連接。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_use_template",
        title: "套用筆記模板",
        trigger: "套用模板、用模板建立、從模板新增、用範本、套模板、use template、apply template、create from template、從範本建立、幫我用模板、用模板寫、我有模板嗎、有什麼模板、查看模板、列出模板",
        behavior: "步驟1：呼叫 @[list_structure]（path 填 \"templates\"）列出可用的模板清單。若資料夾不存在，改呼叫 @[search_vault]（query 填 \"template\"）搜尋模板筆記。步驟2：根據使用者需求選出最符合的模板，呼叫 @[read_note] 讀取模板內容。步驟3：將模板中的佔位符（如 {{日期}}、{{標題}}）替換為使用者提供的資訊（日期從系統時間取得）。步驟4：呼叫 @[plan_announce] 告知將建立的路徑與填入後的內容摘要。步驟5：使用者確認後呼叫 @[create_note] 建立筆記。",
        injection_mode: "passive",
        seed_version: 1,
    },
    BuiltinSkill {
        id: "builtin_email_summary",
        title: "Gmail 郵件整理/摘要",
        trigger: "整理郵件、看今天的郵件、新郵件、未讀郵件、幫我看信、幫我整理信、查信、有什麼信、email summary、check email、today's email、unread emails、read my email、what emails did I get、郵件摘要、有哪些信、最新郵件",
        behavior: "【鐵則，不可違反】\
⚠ 禁止捏造郵件正文、金額、日期、連結或任何數字。\
⚠ 禁止要求使用者提供郵件 ID——使用者不知道郵件 ID。你必須自行從 list_emails 結果中提取 id 欄位。\
---\
步驟1：若對話歷史中尚無 list_emails 工具結果，呼叫 @[list_emails]（query=`is:inbox newer_than:1d`，max_results=20）。若對話歷史已有 list_emails 結果，絕對不可再次呼叫——直接從歷史結果進行步驟2。\
步驟2（判斷意圖）：\
• 若使用者是「整理/摘要今天的郵件」、「有什麼信」、「看今天的信」等未指定特定郵件的請求——直接用 list_emails 結果的 subject/from/date 整理成條列式摘要回答，不需要呼叫 @[read_email]。\
• 若使用者明確指定要看某封郵件的內容（給了主旨、寄件者或任何描述）——從 list_emails 結果中比對，取出對應的 id 欄位（16 位十六進位），立即呼叫 @[read_email]（message_id 填該 id）。若不確定是哪封，選最相符的一封直接呼叫，不要詢問。\
步驟3：若呼叫了 @[read_email]，根據其真實回傳內容回答，不得自行補充或改寫郵件內容。\
若 Gmail 未連接，告知前往設定連接。",
        injection_mode: "passive",
        seed_version: 10,
    },
];

const AGENTS: &[BuiltinAgent] = &[
    BuiltinAgent {
        id: "builtin_skill_builder",
        name: "技能建立助理",
        description: "根據知識描述自動設計並建立 Agent 技能規範",
        kind: "sub",
        tool_names: &[],
        use_skill_pass: true,
        enable_think: false,
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
        tool_names: &[],
        use_skill_pass: true,
        enable_think: false,
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
        tool_names: &[],
        use_skill_pass: true,
        enable_think: false,
        system_prompt: "你是筆記卡片助理，專門根據知識內容生成結構化的筆記卡片。\n\
            ## 卡片模板類型\n\
            - **concept**：概念定義卡，包含定義、詳細說明、範例\n\
            - **procedure**：操作步驟卡，包含前提條件、步驟清單、注意事項\n\
            - **reference**：參考資料卡，包含摘要、重要連結、關鍵點清單\n\
            ## 工作流程\n\
            1. 若使用者指定筆記，呼叫 search_vault 或 read_note 取得原始內容。\n\
            2. 分析內容，決定適合哪些模板類型（通常 2-3 張）。\n\
            3. 列出將建立的卡片清單，請使用者確認。\n\
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
        tool_names: &[],
        use_skill_pass: true,
        enable_think: false,
        system_prompt: "你是筆記整理助理，專門閱讀多篇筆記並產出摘要或結構化整理。\n\
            ## 工作流程\n\
            1. 用 search_vault 或 list_notes_in_folder 取得相關筆記清單。\n\
            2. 對清單中每篇筆記呼叫 read_note 讀取完整內容。\n\
            3. 彙整後輸出結構化摘要（分節標題、要點、結論）。\n\
            4. 若使用者希望儲存整理成果，先說明計畫請使用者確認，確認後再 create_note 或 update_note 寫入。\n\
            ## 注意\n\
            - 摘要要忠實反映筆記原意，不要自行發明內容。\n\
            - 若筆記數量超過 10 篇，先列出清單讓使用者確認範圍再逐一閱讀。",
        max_rounds: 8,
        trigger: "整理筆記、摘要多篇、幫我歸納、彙整資料、總結所有、整合多篇、summarize notes、consolidate、把這些筆記整理、歸納重點、統整一下、彙整成一篇、做個摘要、整理一份報告",
    },
    BuiltinAgent {
        id: "builtin_live_chat",
        name: "live_chat",
        description: "語音對話助理，以口語化方式回覆並支援工具查詢",
        kind: "chat",
        tool_names: &[],
        use_skill_pass: true,
        enable_think: true,
        system_prompt: "你是語音助理，使用者透過語音與你對話。\
            回覆規則：口語化繁體中文，簡短自然，不用 Markdown 或列點。\
            若使用者要求操作筆記（建立/修改/刪除），直接執行工具並簡短告知結果。",
        max_rounds: 4,
        trigger: "",
    },
    BuiltinAgent {
        id: "builtin_chat",
        name: "chat",
        description: "通用筆記助理，可使用工具直接操作 vault",
        kind: "chat",
        tool_names: &[],
        use_skill_pass: true,
        enable_think: true,
        system_prompt: "你是一個筆記助理，可以直接使用工具完成使用者的請求。\n\
            可用工具：\n\
            - 讀取：search_vault、read_note、list_structure、query_memory\n\
            - 開啟：open_note\n\
            - 寫入：create_note、update_note、append_to_note、create_folder\n\
            - 刪除/移動：delete_note、delete_folder、move_note\n\
            規則：\n\
            1. 使用者要「打開」某筆記 → 先 search_vault 找到路徑，再 open_note 打開。\n\
            2. 使用者要「搜尋」或「查看內容」→ search_vault 再 read_note。\n\
            3. 禁止虛構筆記名稱或路徑；搜尋無結果時直接告知找不到。",
        max_rounds: 20,
        trigger: "",
    },
];

/// 為指定 account 幂等更新所有內建 skills 與 agent definitions。
///
/// Skills 使用 per-skill seed_version 比對：
/// - 若 skill_id 不存在 → INSERT
/// - 若 seed_skill_version 不符 → DELETE this skill + INSERT（targeted rebuild）
/// - 若版本相同 → skip
///
/// 要在 release 中強制更新某個 skill 的內容，只需將該 skill 的 `seed_version` +1。
pub async fn seed_builtins(db: &SurrealDb, account_id: &str) {
    // ── Fetch existing builtin skills: skill_id + version ────────────────────
    #[derive(serde::Deserialize)]
    struct SkillRow { skill_id: String, seed_skill_version: Option<u32> }

    let existing: std::collections::HashMap<String, u32> = db
        .query("SELECT skill_id, seed_skill_version FROM agent_skills \
                WHERE account_id = $aid AND knowledge_item_id = '__builtin__'")
        .bind(("aid", account_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<SkillRow>>(0).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.skill_id, r.seed_skill_version.unwrap_or(0)))
        .collect();

    // ── Targeted skill update ────────────────────────────────────────────────
    let now = Utc::now().timestamp();
    let mut inserted = 0usize;
    let mut updated  = 0usize;

    for s in SKILLS {
        match existing.get(s.id).copied() {
            Some(v) if v == s.seed_version => continue, // up-to-date, skip
            Some(_) => {
                // Stale: delete this specific skill then re-insert.
                let _ = db.query(
                    "DELETE agent_skills WHERE account_id = $aid AND skill_id = $sid \
                     AND knowledge_item_id = '__builtin__'"
                )
                .bind(("aid", account_id.to_string()))
                .bind(("sid", s.id.to_string()))
                .await;
                updated += 1;
            }
            None => { inserted += 1; } // Missing: just insert
        }

        if let Err(e) = db.query(
            "CREATE agent_skills CONTENT { \
                skill_id: $sid, account_id: $aid, knowledge_item_id: '__builtin__', \
                title: $title, trigger: $trig, behavior: $behavior, \
                is_active: true, injection_mode: $imode, agent_scope: 'all', \
                trigger_count: 0, tool_chain_order: [], created_at: $now, \
                seed_skill_version: $ver \
            }"
        )
        .bind(("sid",     s.id.to_string()))
        .bind(("aid",     account_id.to_string()))
        .bind(("title",   s.title.to_string()))
        .bind(("trig",    s.trigger.to_string()))
        .bind(("behavior",s.behavior.to_string()))
        .bind(("imode",   s.injection_mode.to_string()))
        .bind(("now",     now))
        .bind(("ver",     s.seed_version))
        .await {
            tracing::error!("[seeds] INSERT skill '{}' failed: {}", s.id, e);
        }
    }

    if inserted > 0 || updated > 0 {
        tracing::info!(
            "seed_builtins: account {} — {} skills inserted, {} skills updated",
            account_id, inserted, updated
        );
    }

    // ── Agent Definitions: INSERT missing ones (preserve user edits) ──────────
    // Fetch existing def_ids for this account in one query, then insert only missing ones.
    // Users may customise system_prompt / tool_names via the frontend; we never overwrite.
    #[derive(serde::Deserialize)]
    struct DefIdRow { def_id: String }
    let existing_defs: std::collections::HashSet<String> = db
        .query("SELECT def_id FROM agent_definitions WHERE account_id = $aid")
        .bind(("aid", account_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<DefIdRow>>(0).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.def_id)
        .collect();

    let mut agents_inserted = 0usize;
    for a in AGENTS {
        if existing_defs.contains(a.id) { continue; }
        let tool_names_json: serde_json::Value = serde_json::Value::Array(
            a.tool_names.iter().map(|t| serde_json::Value::String(t.to_string())).collect(),
        );
        let payload = serde_json::json!({
            "def_id":         a.id,
            "account_id":     account_id,
            "name":           a.name,
            "description":    a.description,
            "kind":           a.kind,
            "skill_ids":      [],
            "tool_names":     tool_names_json,
            "use_skill_pass": a.use_skill_pass,
            "enable_think":   a.enable_think,
            "system_prompt":  a.system_prompt,
            "max_rounds":     a.max_rounds,
            "is_active":      true,
            "is_builtin":     true,
            "trigger":        a.trigger,
            "status":         "active",
            "use_count":      0,
            "created_at":     now,
        });
        if let Err(e) = db.query("CREATE agent_definitions CONTENT $data")
            .bind(("data", payload))
            .await
        {
            tracing::error!("[seeds] INSERT agent_definitions '{}' failed: {}", a.name, e);
        } else {
            agents_inserted += 1;
        }
    }
    if agents_inserted > 0 {
        tracing::info!("seed_builtins: account {} — {} agent defs inserted", account_id, agents_inserted);
    }

    // ── Harness built-ins: conditional INSERT ────────────────────────────────
    crate::service::harness::seed_agents::seed_builtin_agents(db, account_id).await;
    crate::service::harness::eval::cases::seed_eval_cases(db, account_id).await;

    // Embedding happens later when embed_skills_for_account is called with embedding_url.
    // (seed_builtins doesn't have access to the daemon's embedding_url)

    // Ensure periodic memory-agent scheduled tasks exist for every vault this account owns
    #[derive(serde::Deserialize)]
    struct VaultRow { vault_id: String }
    let vaults: Vec<VaultRow> = db
        .query("SELECT vault_id FROM vaults WHERE account_id = $aid")
        .bind(("aid", account_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<VaultRow>>(0).ok())
        .unwrap_or_default();

    for v in vaults {
        ensure_memory_schedule(db, &v.vault_id, account_id).await;
    }
}

/// Idempotently ensures a periodic memory_agent scheduled task exists for (vault_id, account_id).
/// Creates one if absent; does nothing if one already exists.
pub async fn ensure_memory_schedule(db: &SurrealDb, vault_id: &str, account_id: &str) {
    // Check if a memory_agent task already exists for this vault+account
    #[derive(serde::Deserialize)]
    struct Row { #[allow(dead_code)] task_id: String }
    let existing: Vec<Row> = db
        .query("SELECT task_id FROM scheduled_tasks WHERE vault_id = $vid AND account_id = $aid AND agent_def_name = 'memory_agent' LIMIT 1")
        .bind(("vid", vault_id.to_string()))
        .bind(("aid", account_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
        .unwrap_or_default();

    if !existing.is_empty() { return; }

    let task_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().timestamp();
    let run_at = now + 28800; // first run in 8 hours

    let _ = db
        .query("INSERT INTO scheduled_tasks \
                (task_id, vault_id, account_id, description, agent_def_name, agent_prompt, \
                 run_at_ts, repeat_interval_secs, status, created_at) \
                VALUES ($tid, $vid, $aid, '定期記憶整理', 'memory_agent', NONE, \
                        $run_at, 28800, 'pending', $now)")
        .bind(("tid",    task_id))
        .bind(("vid",    vault_id.to_string()))
        .bind(("aid",    account_id.to_string()))
        .bind(("run_at", run_at))
        .bind(("now",    now))
        .await;

    tracing::info!("[seeds] created memory_agent schedule for vault={} account={}", vault_id, account_id);
}

/// Seed the `templates/` folder and starter template notes inside a vault.
///
/// Idempotent: only runs if the `templates/` directory does not yet exist in
/// the vault root.  This lets users who already have a templates folder keep
/// their customisations untouched.
pub async fn seed_vault_templates(vault_path: &str) {
    if vault_path.is_empty() { return; }
    let templates_dir = std::path::Path::new(vault_path).join("templates");
    if templates_dir.exists() { return; }

    if let Err(e) = std::fs::create_dir_all(&templates_dir) {
        tracing::warn!("[seeds] failed to create templates/ dir in {}: {}", vault_path, e);
        return;
    }

    // ── Starter templates ────────────────────────────────────────────────────
    let templates: &[(&str, &str)] = &[
        ("會議紀錄.md", "\
# 會議紀錄：{{標題}}

**日期：** {{日期}}
**與會人員：** {{人員}}
**主持人：** {{主持人}}

---

## 議程

1.

## 討論紀錄

### 主題一：

### 主題二：

## 決議事項

- [ ]

## 下次會議

**時間：**
**預計議題：**
"),
        ("每日日記.md", "\
# {{日期}} 日記

## 今天完成了什麼

-

## 遇到的問題

-

## 明天的計劃

- [ ]

## 隨筆

"),
        ("專案計劃.md", "\
# 專案：{{專案名稱}}

**開始日期：** {{日期}}
**預計完成：**
**負責人：**

---

## 目標

> 一句話說明這個專案要達成什麼。

## 範圍

### In Scope
-

### Out of Scope
-

## 里程碑

| 里程碑 | 預計日期 | 狀態 |
|--------|----------|------|
|        |          | 待開始 |

## 任務清單

- [ ]

## 備注

"),
        ("讀書筆記.md", "\
# 讀書筆記：{{書名}}

**作者：** {{作者}}
**閱讀日期：** {{日期}}
**評分：** ⭐⭐⭐⭐⭐

---

## 核心主題

## 重點摘要

1.

## 印象深刻的段落

>

## 個人反思

## 行動項目

- [ ]

## 相關筆記

-
"),
    ];

    for (filename, content) in templates {
        let file_path = templates_dir.join(filename);
        if let Err(e) = std::fs::write(&file_path, content.as_bytes()) {
            tracing::warn!("[seeds] failed to write template {}: {}", filename, e);
        }
    }

    tracing::info!("[seeds] seeded {} templates in {}", templates.len(), vault_path);
}

/// Embed all skills for an account that have no embedding yet.
/// `embedding_url` — same URL used by the embedder service (e.g. llama.cpp /embedding).
/// Called after seed_builtins (from auth routes that have access to daemon state)
/// and after any INSERT/UPDATE on agent_skills.
pub async fn embed_skills_for_account(
    db: &SurrealDb,
    account_id: &str,
    embedding_url: &Option<String>,
) {
    let Some(_) = embedding_url else {
        tracing::debug!("[seeds] embedding_url not set, skipping skill embedding");
        return;
    };

    #[derive(serde::Deserialize)]
    struct SkillRow {
        skill_id: String,
        title: String,
        trigger: String,
        behavior: String,
    }
    let rows: Vec<SkillRow> = db
        .query("SELECT skill_id, title, trigger, behavior FROM agent_skills \
                WHERE account_id = $aid AND embedding IS NONE")
        .bind(("aid", account_id.to_string()))
        .await
        .ok()
        .and_then(|mut r| r.take(0).ok())
        .unwrap_or_default();

    if rows.is_empty() { return; }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let count = rows.len();
    for row in rows {
        let text = format!("{} {} {}",
            row.title,
            row.trigger,
            row.behavior.chars().take(200).collect::<String>(),
        );
        if let Some(vec) = crate::embedding::embedder::embed_text(&client, embedding_url, &text).await {
            let _ = db
                .query("UPDATE agent_skills SET embedding = $emb WHERE skill_id = $sid")
                .bind(("emb", vec))
                .bind(("sid", row.skill_id))
                .await;
        }
    }
    tracing::info!("[seeds] embedded {} skills for account {}", count, account_id);
}
