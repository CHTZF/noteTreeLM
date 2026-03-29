/// runtime/tool_schema.rs
///
/// 所有工具的 JSON Schema 定義（OpenAI function-calling 格式）。
/// 主要輸出：
///   - `vault_tools()`              — 完整工具列表
///   - `filter_vault_tools_by_names()` — 依名稱過濾子集

/// 工具定義（OpenAI function calling 格式）
pub fn vault_tools() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "search_vault",
                "description": "全文搜索 Vault 中的筆記，返回相關筆記列表及摘要。\
【前置工具】：open_note / read_note / update_note / append_to_note / delete_note / move_note 都需要精確路徑，若路徑不確定，必須先呼叫 search_vault 取得。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "搜索關鍵字" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_structure",
                "description": "列出指定資料夾路徑下的子資料夾和筆記（.md）。path 傳空字串表示 Vault 根目錄。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "相對於 Vault 根目錄的資料夾路徑（空字串 = 根目錄）" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_note",
                "description": "讀取指定筆記的完整 Markdown 內容，用於需要分析、摘要或修改筆記內容時。\
【前置要求】必須知道精確路徑；不確定時先用 search_vault。\
注意：若使用者只是要「打開」或「查看」筆記，請改用 open_note 工具；read_note 僅用於需要理解或修改內容的情況。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "筆記相對路徑（含 .md 副檔名，例如 工作/專案A.md）" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_note",
                "description": "在 Vault 中建立新筆記，會自動建立所需的父資料夾",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "新筆記的相對路徑（含 .md 副檔名）" },
                        "content": { "type": "string", "description": "筆記的 Markdown 內容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_note",
                "description": "覆寫更新現有筆記的完整內容。\
【操作序列】：(1) 若路徑不確定 → 先 search_vault；(2) 若需保留現有內容做部分修改 → 先 read_note 取得原始內容，再修改後呼叫 update_note。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "筆記相對路徑" },
                        "content": { "type": "string", "description": "新的完整 Markdown 內容" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_folder",
                "description": "在 Vault 中建立新資料夾（含所有中間層資料夾）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "新資料夾的相對路徑" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "在網路上搜尋最新資訊。當本地知識庫缺乏相關內容、或需要最新資訊時使用。\
搜尋結果會自動在背景加入「匯入知識」，使用者稍後可在匯入中心查看完整來源。\
不要用來查詢 Vault 筆記（請用 search_vault）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "搜尋關鍵字或問題（建議使用具體關鍵字）"
                        }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "plan_announce",
                "description": "當你打算執行寫入操作（create_note / update_note / create_folder）且需要使用者確認時，\
先呼叫此工具記錄計畫。提供使用者可能用來確認/取消/中斷的樣本短語（用於語意匹配），\
以及你打算執行的工具清單（deferred_tools）。呼叫後再用文字告知使用者計畫內容，等待確認。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "confirm_phrases": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "使用者確認計畫時可能說的 10-15 個短語（口語、正式、縮短形式都要）"
                        },
                        "cancel_phrases": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "使用者取消計畫時可能說的 10-15 個短語"
                        },
                        "interrupt_phrases": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "使用者暫停/插話時可能說的短語"
                        },
                        "deferred_tools": {
                            "type": "array",
                            "description": "計畫執行的工具清單",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "args": {"type": "object"}
                                },
                                "required": ["name", "args"]
                            }
                        }
                    },
                    "required": ["confirm_phrases", "cancel_phrases", "interrupt_phrases", "deferred_tools"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "open_note",
                "description": "在筆記編輯器中打開（切換至）指定筆記，讓使用者在編輯器中直接看到內容。\
使用者說「打開」「開啟」「跳轉到」「要查看」「幫我看」「看一下」某筆記時，優先使用此工具，不要用 read_note。\
若不確定路徑，先用 search_vault 找到路徑再呼叫。呼叫後只需回覆「已打開 xxx 筆記」，不要輸出任何筆記內容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "筆記的相對路徑，例如 'folder/note.md'"
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_recent_conversations",
                "description": "讀取最近的對話記錄，分析使用者的重複需求、知識缺口和行為模式。僅供自我改進分析使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "number",
                            "description": "要讀取的對話數量（預設 10，最多 20）"
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_agent_skill",
                "description": "根據觀察到的使用者模式，建立新的技能規範。建立後預設未啟用，使用者可在「我的技能規範」頁面審核並啟用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "技能名稱" },
                        "trigger": { "type": "string", "description": "觸發條件，以「當...時」開頭" },
                        "behavior": { "type": "string", "description": "具體操作規範：先做A，再做B" },
                        "injection_mode": {
                            "type": "string",
                            "enum": ["passive", "active"],
                            "description": "passive=語意相似時注入；active=永遠注入"
                        },
                        "need_tool_chain": {
                            "type": "boolean",
                            "description": "工具是否需要嚴格依序執行（有前置條件時設為 true）"
                        },
                        "tool_chain_order": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "工具執行順序（need_tool_chain=true 時填入），例如 [\"search_vault\", \"read_note\", \"update_note\"]"
                        }
                    },
                    "required": ["title", "trigger", "behavior"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "touch_agent",
                "description": "當使用者需要以下任何一種任務時，必須呼叫此工具：\
網路搜尋、即時資訊（天氣/新聞/股價）、外部 API、複雜計算、程式碼生成、\
資料分析、建立或修改筆記、整理或摘要多篇筆記。\
系統會自動以 task 語意搜尋現有 agent；找到則複用，找不到則自動建立後執行。\
只有「純粹閒聊」或「解釋概念」才可不呼叫此工具直接回答。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "完整任務描述，包含所有必要資訊（用於語意匹配與執行）"
                        },
                        "name": {
                            "type": "string",
                            "description": "（可選）agent 名稱提示，建立新 agent 時使用"
                        },
                        "description": {
                            "type": "string",
                            "description": "（可選）agent 職責描述，建立新 agent 時使用"
                        },
                        "trigger": {
                            "type": "string",
                            "description": "（可選）pre-routing 觸發關鍵詞，建立新 agent 時使用"
                        },
                        "tool_names": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "（可選）建立新 agent 時建議使用的工具"
                        },
                        "context": {
                            "type": "string",
                            "description": "（可選）提供給 agent 的背景資訊"
                        }
                    },
                    "required": ["task"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "call_agent",
                "description": "（內部使用）透過 System Agent Service 路由任務給指定 agent。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "target": { "type": "string" },
                        "task": { "type": "string" },
                        "context": { "type": "string" }
                    },
                    "required": ["target", "task"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "create_agent",
                "description": "（內部使用）建立 agent definition。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "trigger": { "type": "string" },
                        "tool_names": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["name", "description", "trigger", "tool_names"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_available_agents",
                "description": "列出目前所有可用的 agent definitions（包含自訂）。",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_memory",
                "description": "搜尋過去對話記憶。keywords 空陣列=取最新記憶；有關鍵字=語意相似度搜尋（向量搜尋）。since 為時間下限 YYYY-MM-DD。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "keywords": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "關鍵字列表（空陣列 = 取最新記憶）"
                        },
                        "since": {
                            "type": "string",
                            "description": "時間下限，YYYY-MM-DD 格式（可選）"
                        },
                        "limit": {
                            "type": "number",
                            "description": "最多返回幾條記憶（預設 5）"
                        }
                    },
                    "required": ["keywords"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "prefetch_memory",
                "description": "根據當前對話主題，自動擷取最相關的記憶事實並注入為背景知識。通常由系統自動呼叫，不需手動使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "context": {
                            "type": "string",
                            "description": "當前對話的關鍵詞或主題描述（可選，留空則取最新記憶）"
                        }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "think",
                "description": "在執行下一個工具前，輸出一句內心獨白描述你正在思考的方向。必須在每個工具呼叫之前先呼叫此工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "thought": {
                            "type": "string",
                            "description": "內心獨白，口語化繁體中文，10字以內，描述你接下來要做什麼或想到什麼"
                        }
                    },
                    "required": ["thought"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_related",
                "description": "透過知識圖譜找出與指定筆記相關聯的筆記（wiki link 連結）。\
適用情境：探索某個主題的延伸閱讀、找出相互引用的筆記群。\
【操作序列】：先 list_structure 確認路徑 → 呼叫 find_related 取得相關節點 → 視需要 read_note 閱讀內容。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "起點筆記的相對路徑（含 .md 副檔名）"
                        },
                        "depth": {
                            "type": "integer",
                            "description": "圖譜遍歷深度（預設 1，最大 2）"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "最多回傳幾個相關筆記（預設 10）"
                        }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_current_datetime",
                "description": "取得目前本地時間（年月日時分秒時區）",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_notes_in_folder",
                "description": "列出指定資料夾下的所有筆記。\
【操作序列】：若資料夾路徑不確定 → 先 list_structure 確認資料夾名稱，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "folder": {"type": "string", "description": "資料夾相對路徑（如 'projects' 或 'projects/web'）"}
                    },
                    "required": ["folder"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "append_to_note",
                "description": "在現有筆記末尾追加內容（不覆蓋原有內容）。\
【操作序列】：若路徑不確定 → 先 search_vault 取得路徑，再呼叫 append_to_note。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑"},
                        "content": {"type": "string", "description": "要追加的內容"}
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_note",
                "description": "刪除指定筆記（永久，不可復原）。\
【操作序列】：操作不可逆，若路徑不確定，必須先 search_vault 確認路徑後再呼叫。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑或名稱"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "delete_folder",
                "description": "刪除指定資料夾及其所有內容（需使用者確認）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "資料夾相對路徑"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "move_note",
                "description": "移動或重新命名筆記。\
【操作序列】：若 from 路徑不確定 → 先 search_vault 找到來源路徑，再呼叫 move_note。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from": {"type": "string", "description": "原始相對路徑"},
                        "to": {"type": "string", "description": "目標相對路徑（含新檔名）"}
                    },
                    "required": ["from", "to"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "show_toast",
                "description": "顯示通知訊息給使用者（適合背景任務完成後通知）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {"type": "string"},
                        "kind": {"type": "string", "description": "info|success|warning|error", "enum": ["info","success","warning","error"]},
                        "duration_ms": {"type": "integer", "description": "顯示時間（毫秒），預設 3000"}
                    },
                    "required": ["message"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ui_action",
                "description": "模擬使用者操作 UI（切換 tab、開啟搜尋等）",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "操作類型",
                            "enum": ["open_tab","focus_editor","open_search","new_note","open_settings","scroll_to_top"]
                        },
                        "payload": {"type": "object", "description": "額外參數（如 open_tab 需要 tab 名稱）"}
                    },
                    "required": ["action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "reflect_on_skills",
                "description": "查看所有技能規範的觸發命中率，供 agent 自我調優",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_skills",
                "description": "當使用者的請求沒有自動觸發技能時，主動搜尋語意相似的技能規範。\
將你對使用者意圖的理解概括為簡短的 use_ask（標準化意圖，非原文）。\
例如：使用者說「今天台北天氣如何」→ use_ask 為「查詢天氣」；「幫我 Google 一下新聞」→ use_ask 為「搜尋網路新聞」。\
找到匹配技能後，請依照技能的 behavior 執行任務。\
若沒有匹配技能，直接回應使用者即可。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "use_ask": {
                            "type": "string",
                            "description": "使用者意圖的標準化概括（簡短、通用），用於語意搜尋技能庫。例如「查詢天氣」、「搜尋新聞」、「整理筆記」"
                        }
                    },
                    "required": ["use_ask"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_web",
                "description": "搜尋網路（使用 Brave Search），取得即時資訊",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "搜尋關鍵字"}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "schedule_task",
                "description": "排程一個任務，在指定時間執行（可設定重複間隔）。若需排程執行 agent，填入 agent_def_name。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "description": {"type": "string", "description": "任務描述（到時會顯示通知）"},
                        "run_at": {"type": "string", "description": "執行時間，ISO 8601 格式（如 2026-03-21T09:00:00+08:00）"},
                        "repeat_interval_seconds": {"type": "integer", "description": "重複間隔秒數，0 或省略表示只執行一次"},
                        "agent_def_name": {"type": "string", "description": "要執行的 agent 名稱（如 'memory_agent'），省略則只顯示通知"},
                        "agent_prompt": {"type": "string", "description": "傳給 agent 的初始提示（可選）"},
                        "account_id": {"type": "string", "description": "目前使用者的 account_id"}
                    },
                    "required": ["description", "run_at"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "compress_to_knowledge",
                "description": "主動將對話中的重要洞見、結論或知識儲存到 Vault 的 knowledge/ 資料夾。\
當對話產生了值得長期保存的見解時，主動呼叫此工具（不需等使用者要求）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "知識標題（簡潔，不超過 30 字）"},
                        "content": {"type": "string", "description": "要儲存的知識內容（Markdown 格式）"},
                        "tags": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "標籤（如 ['ai', 'productivity']），可選"
                        }
                    },
                    "required": ["title", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_note_frontmatter",
                "description": "局部更新筆記的 YAML frontmatter 欄位，不覆蓋正文內容。\
適合只更新 tags、status、priority 等屬性而不想修改筆記正文時使用。\
【操作序列】：若路徑不確定 → 先 search_vault 取得精確路徑，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑（含 .md）"},
                        "fields": {
                            "type": "object",
                            "description": "要更新的欄位（鍵值對），例如 {\"status\": \"done\", \"tags\": [\"project\", \"done\"]}"
                        }
                    },
                    "required": ["path", "fields"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_similar_notes",
                "description": "用語意向量搜尋找出與查詢最相似的筆記。\
適合探索相關主題、查找知識重複、或發現潛在關聯時使用。\
與 search_vault 不同：search_vault 做關鍵字全文搜索；find_similar_notes 做向量語意相似度搜索。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "語意搜尋查詢（可以是句子或概念描述）"},
                        "limit": {"type": "number", "description": "返回結果數量（預設 5，最多 20）"}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "summarize_note_collection",
                "description": "批次讀取多篇指定筆記，並由 LLM 生成整合摘要。\
適合需要對特定筆記集合做深度分析或總結時使用。\
【操作序列】：先用 search_vault 或 list_notes_in_folder 取得路徑列表，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "筆記路徑陣列（相對路徑）"
                        },
                        "query": {"type": "string", "description": "摘要的聚焦重點（可選），例如「主要結論」、「行動項目」"}
                    },
                    "required": ["paths"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "distill_preferences",
                "description": "分析過去對話記憶，萃取使用者的工作習慣、偏好模式與常見需求。\
適合使用者詢問「你了解我的習慣嗎」或需要個人化建議前的準備步驟。",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_note_backlinks",
                "description": "查詢哪些筆記連結至指定筆記（反向連結）。\
用於了解知識圖譜中的關聯性，或找出引用某篇筆記的所有來源。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "目標筆記的相對路徑（含 .md）"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_vault_stats",
                "description": "取得知識庫的整體統計資料：筆記總數、資料夾數、總字數、最近修改的筆記。\
適合使用者詢問知識庫概況，或需要對知識庫健康度做評估時使用。",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_by_tag",
                "description": "以 frontmatter tag 標籤過濾筆記。\
比 search_vault 更精準：當使用者明確說「給我標籤是 X 的筆記」時使用此工具。\
標籤名稱不區分大小寫。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "tag": {"type": "string", "description": "要搜尋的標籤名稱（如 'project'、'done'、'reading'）"},
                        "limit": {"type": "number", "description": "最多返回幾篇（預設 50）"}
                    },
                    "required": ["tag"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "extract_action_items",
                "description": "從筆記（或整個資料夾）中提取待辦事項：包含 `- [ ]` checkbox、`TODO:`、`ACTION:`、`FIXME:` 標記。\
適合使用者說「幫我整理一下有什麼待辦」或「這個資料夾有哪些 TODO」時使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "單一筆記的相對路徑（path 和 folder 二擇一）"},
                        "folder": {"type": "string", "description": "掃描整個資料夾的路徑（path 和 folder 二擇一）"},
                        "include_done": {"type": "boolean", "description": "是否包含已完成的 [x] 項目（預設 false）"}
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_orphan_notes",
                "description": "找出知識庫中沒有任何反向連結的孤立筆記（沒有任何其他筆記引用它們）。\
適合做知識庫健康診斷，找出遺忘或未整合的筆記。\
執行後建議配合 find_similar_notes + link_notes 建立連結。",
                "parameters": {"type": "object", "properties": {}, "required": []}
            }
        },
        {
            "type": "function",
            "function": {
                "name": "list_recent_notes",
                "description": "列出最近 N 天內修改的筆記，按修改時間排序。\
適合使用者問「最近在寫什麼」或「這週修改了哪些筆記」時使用。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "days": {"type": "number", "description": "往回查幾天（預設 7）"},
                        "limit": {"type": "number", "description": "最多返回幾篇（預設 20）"}
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "extract_note_links",
                "description": "取出筆記中所有出向的 [[wiki link]] 連結，用於分析知識圖譜的出向連結。\
與 get_note_backlinks（反向）相對，這是正向（此筆記連到哪裡）。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "筆記相對路徑（含 .md）"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "link_notes",
                "description": "在筆記 A（from_path）中插入指向筆記 B（to_path）的 [[wiki link]]。\
若 from_path 已有 Related/Links/相關 章節則插入其中，否則自動在末尾新增 ## Related 章節。\
【操作序列】：若路徑不確定 → 先 search_vault 取得精確路徑，再呼叫本工具。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "from_path": {"type": "string", "description": "要插入連結的筆記路徑（被修改方）"},
                        "to_path": {"type": "string", "description": "要被連結到的目標筆記路徑"},
                        "section": {"type": "string", "description": "插入到指定章節名稱（可選，如 'Related'、'See Also'）"}
                    },
                    "required": ["from_path", "to_path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "generate_moc",
                "description": "為指定資料夾自動生成 Map of Contents（MOC）索引筆記。\
輸出包含資料夾內所有筆記的 [[wiki link]] 清單，按子資料夾分組。\
預設輸出至 {folder}/index.md，也可指定 output_path。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "folder": {"type": "string", "description": "要生成 MOC 的資料夾路徑（如 'projects' 或 'notes/2026'）"},
                        "output_path": {"type": "string", "description": "MOC 筆記的輸出路徑（可選，預設 {folder}/index.md）"}
                    },
                    "required": ["folder"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "live_respond",
                "description": "【必須最後呼叫】語音對話的結構化回應工具。\
完成所有資訊蒐集或操作後，必須呼叫此工具輸出最終回覆。\
speech 欄位的內容會被 TTS 朗讀，必須是自然口語、不含 Markdown。\
action 決定前端行為：\
none=只 TTS；show_results=顯示筆記清單；open_note=在編輯器開啟筆記；\
open_tab=切換頁籤；show_error=顯示錯誤卡片。",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "speech": {
                            "type": "string",
                            "description": "TTS 朗讀文字，必須是口語化繁體中文（或依語言設定），2-3 句以內，不含 Markdown 或列點"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["none", "show_results", "open_note", "open_tab", "show_error"],
                            "description": "前端動作類型"
                        },
                        "content": {
                            "type": "string",
                            "description": "action=show_results 時顯示在畫面上的詳細內容（網頁摘要、筆記內容等）；speech 只說短句，詳細資訊放這裡"
                        },
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "action=show_results 時的筆記路徑列表（vault 筆記用，與 content 擇一或並用）"
                        },
                        "path": {
                            "type": "string",
                            "description": "action=open_note 時的單一筆記路徑"
                        },
                        "tab": {
                            "type": "string",
                            "description": "action=open_tab 時的頁籤名稱（settings/trash/agents/skills）"
                        },
                        "error": {
                            "type": "string",
                            "description": "action=show_error 時的錯誤訊息"
                        }
                    },
                    "required": ["speech", "action"]
                }
            }
        }
    ])
}

/// 從 vault_tools() 中過濾出指定名稱的工具子集。
/// plan_announce 永遠包含（寫入確認機制必需）。
pub fn filter_vault_tools_by_names(names: &[String]) -> serde_json::Value {
    const ALWAYS_INCLUDE: &[&str] = &["plan_announce"];
    let name_set: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    let filtered: Vec<serde_json::Value> = vault_tools()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| {
            let n = t["function"]["name"].as_str().unwrap_or("");
            name_set.contains(n) || ALWAYS_INCLUDE.contains(&n)
        })
        .collect();
    serde_json::Value::Array(filtered)
}

/// 余弦相似度（兩向量長度不同或為空時回傳 0.0）
#[allow(dead_code)]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { return 0.0; }
    dot / (norm_a * norm_b)
}
