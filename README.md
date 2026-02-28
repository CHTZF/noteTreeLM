# noteTreeLM

A local-first, privacy-focused knowledge base and note-taking desktop application built with Tauri 2 + React + TypeScript. Provides an Obsidian-like experience with graph visualization, wikilinks, full-text search, voice transcription, and local AI integration — all data stays on your machine.

---

## Features

- **Markdown editor** with wikilinks, preview, split view, math (KaTeX), and diagrams (Mermaid)
- **Knowledge graph** — automatic graph of notes, links, images, and topics (Cytoscape.js)
- **Full-text search** — FTS5-powered search with BM25 ranking and snippet highlighting
- **Backlinks** — bidirectional link tracking across all notes
- **Voice transcription** — real-time VAD-segmented transcription via local whisper-server
- **Voice post-processing** — optional LLM formatting or YAML frontmatter generation after transcription
- **AI chat** — chat with a local LLM (llama-server), optionally grounding on the current note
- **Vault Agent** — AI with vault tools: search, read, create, edit notes and folders via chat
- **Web import** — clip web pages to Markdown (SSRF-protected)
- **Model manager** — download and manage GGUF models inside the app
- **Drag-and-drop file tree** — hierarchical folder/note management with context menus
- **Soft-delete trash** — restore deleted notes and folders
- **Image asset management** — import images, display inline via `vault://` protocol
- **Auto-save** — configurable (after delay, on focus change, on window change)
- **Onboarding wizard** — first-run vault setup and model download

---

## Tech Stack

| Layer | Technology |
|-------|-----------|
| Desktop shell | Tauri 2 (Rust) |
| Frontend | React 18 + TypeScript |
| State management | Zustand |
| Editor | CodeMirror 6 |
| Graph visualization | Cytoscape.js + fCose layout |
| Markdown rendering | markdown-it + KaTeX + Mermaid |
| Database | SQLite (WAL mode) via sqlx |
| Full-text search | SQLite FTS5 |
| Async runtime | Tokio |
| Voice capture | Web AudioWorklet (16kHz) |
| Speech-to-text | whisper-server (local HTTP) |
| Local LLM | llama-server (OpenAI-compatible HTTP) |
| Build tool | Vite + Tauri CLI |

---

## Project Structure

```
noteTreeLM/
├── src/                            # React frontend
│   ├── App.tsx                     # Main layout, routing, keyboard shortcuts
│   ├── stores/                     # Zustand stores
│   │   ├── vaultStore.ts           # Notes, file tree, vault operations
│   │   ├── editorStore.ts          # Current note, content, view mode
│   │   ├── settingsStore.ts        # App configuration
│   │   ├── graphStore.ts           # Graph data
│   │   ├── navigationStore.ts      # Back/forward history
│   │   └── debugStore.ts           # Debug log entries
│   ├── components/
│   │   ├── Editor/
│   │   │   ├── Editor.tsx          # CodeMirror 6 editor, voice recorder integration
│   │   │   ├── Toolbar.tsx         # Formatting buttons, view toggle
│   │   │   └── PreviewPanel.tsx    # Markdown → HTML rendering
│   │   ├── Sidebar/
│   │   │   └── FileTree.tsx        # Hierarchical file/folder tree with DnD
│   │   ├── Graph/
│   │   │   └── GraphView.tsx       # Cytoscape.js force-directed graph
│   │   ├── Chat/
│   │   │   └── ChatPanel.tsx       # AI chat panel (stream + agent modes)
│   │   ├── Search/
│   │   │   └── SearchPanel.tsx     # FTS5 full-text search UI
│   │   ├── Settings/
│   │   │   └── SettingsModal.tsx   # Settings tabs (General/AI/Voice/Advanced/Raw)
│   │   ├── Backlinks/
│   │   │   └── BacklinksPanel.tsx  # Bidirectional link display
│   │   └── Debug/
│   │       └── DebugPanel.tsx      # Dev log viewer (voice/whisper/llm/chat)
│   ├── hooks/
│   │   └── useVoiceRecorder.ts     # AudioWorklet-based VAD recording
│   ├── worklets/
│   │   └── voice-processor.js      # AudioWorkletProcessor (runs in audio thread)
│   └── types/
│       └── settings.ts             # Settings interface + DEFAULT_SETTINGS
│
├── src-tauri/
│   ├── src/
│   │   ├── main.rs                 # Entry point
│   │   ├── lib.rs                  # Tauri builder, command registry, exit hooks
│   │   ├── state.rs                # AppState: DB pool, server processes, settings
│   │   ├── error.rs                # AppError enum, serialization
│   │   └── commands/
│   │       ├── settings.rs         # get_settings, save_settings, API key ops
│   │       ├── vault.rs            # Note/folder CRUD, trash, assets, scan
│   │       ├── search.rs           # FTS5 search
│   │       ├── graph.rs            # Graph data retrieval
│   │       ├── voice.rs            # Whisper server, transcription, warmup
│   │       ├── ai.rs               # stream_chat, process_with_llm, agent_chat
│   │       ├── import.rs           # Web URL import to Markdown
│   │       └── download.rs         # GGUF model download + progress
│   ├── db/
│   │   ├── init.rs                 # Schema creation, FTS5 triggers
│   │   └── queries.rs              # Typed query functions
│   ├── vault/
│   │   ├── watcher.rs              # File system watcher (notify crate)
│   │   └── indexer.rs              # Vault scan, link parsing, graph building
│   └── migrations/
│       └── 001_initial.sql         # Complete database schema
│
├── tauri.conf.json                 # Tauri permissions, CSP, window config
├── vite.config.ts                  # Vite build config
├── package.json
└── Cargo.toml
```

---

## Architecture Overview

### Data Flow

```
User Action
    │
    ▼
React Component  ──stores──►  Zustand Store
    │                              │
    │ invoke()                     │ invoke()
    ▼                              ▼
Tauri Command (Rust)
    │
    ├──► SQLite (sqlx)        ← notes, links, graph, search, settings, trash
    ├──► tokio::fs             ← file read/write/move on vault directory
    ├──► whisper-server HTTP   ← POST /inference (audio → text)
    └──► llama-server HTTP     ← POST /v1/chat/completions (chat/agent)
    │
    ▼
Result<T, AppError>  ──emit()──►  Tauri Event  ──listen()──►  Frontend
```

### External Processes

The app spawns and manages two persistent local HTTP servers:

**whisper-server** (speech-to-text)
- Port: configurable (default 8081)
- Spawned lazily on first `transcribe_audio` call
- Kept alive for the session; killed on app exit
- Warmed up at startup with a 1-second silent inference
- API: `POST /inference` (multipart WAV)

**llama-server** (local LLM)
- Port: configurable (default 8080)
- Spawned lazily on first `stream_chat` call
- OpenAI-compatible API: `POST /v1/chat/completions`
- Supports streaming (SSE) and non-streaming
- Used for: chat, voice post-processing, vault agent

### Voice Recording Pipeline

```
Microphone
    │  getUserMedia (16kHz mono)
    ▼
AudioContext (16kHz)
    │
    ▼
AudioWorkletNode (voice-processor.js)
    │  128 samples per callback (~8ms)
    │  computes RMS per quantum
    ▼
Main Thread VAD
    │  RMS > 0.01 → speech active
    │  silence > 400ms → flush segment
    ▼
Sequential Queue
    │  one transcription at a time
    ▼
transcribe_audio (Tauri)
    │  PCM f32 → WAV bytes → POST /inference
    ▼
Text Callback
    │
    ├── auto-insert into editor
    └── optional LLM post-processing (format / summary)
```

### AI Chat Modes

**Stream Chat** (default)
- Sends `messages[]` to llama-server `/v1/chat/completions` with `stream: true`
- Emits `llm:token` events as SSE chunks arrive
- Frontend accumulates tokens into streaming bubble

**Agent Chat** (Vault Tools enabled)
- Sends tool definitions to llama-server in function-calling format
- Iterates up to 8 rounds: LLM decides tool → execute → feed result back
- Tools: `search_vault`, `list_structure`, `read_note`, `create_note`, `update_note`, `create_folder`
- Emits `agent:tool_call` events for frontend display
- Final answer returned as string

---

## Database Schema (SQLite)

```sql
-- Core content
notes          (path PK, title, content, frontmatter, word_count, created_at, modified_at, checksum)
links          (id, source_path→notes, target_title, target_path→notes, link_type, raw_text, alias, heading, line_number)

-- Knowledge graph
graph_nodes    (id PK, node_type, label, url, file_path, metadata, created_at)
graph_edges    (id, source_id→graph_nodes, target_id→graph_nodes, edge_type, weight)
topics         (id, name, description, keywords, note_path→notes, created_at, updated_at)
topic_memberships (topic_id→topics, note_path→notes, score)

-- Administrative
settings       (key PK, value, updated_at)
assets         (file_path PK, mime_type, file_size, created_at)
imports        (id, source_url UNIQUE, note_path→notes, imported_at, status, error_message)
trash_items    (id UUID, original_path, name, title, trash_filename, deleted_at)
tags           (note_path→notes, tag)

-- Full-text search (FTS5 virtual table, auto-synced via triggers)
search_fts     (path UNINDEXED, title, content, tags)
```

---

## Settings Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `vault_path` | string | — | Absolute path to vault directory |
| `theme` | `'dark'`\|`'light'` | `'dark'` | UI theme |
| `sidebar_width` | number | 240 | Sidebar width (px) |
| `graph_panel_width` | number | 320 | Right panel width (px) |
| `font_sans` | string | system | UI sans-serif font |
| `font_mono` | string | system | Editor monospace font |
| `editor_font_size` | number | 14 | Editor font (px) |
| `ui_font_size` | number | 14 | UI scale (14 = 100%) |
| `auto_save_mode` | enum | `'afterDelay'` | Auto-save trigger |
| `auto_save_delay` | number | 1000 | Auto-save delay (ms) |
| `whisper_cli_path` | string | — | Path to whisper-server binary |
| `whisper_model_path` | string | — | Path to Whisper `.gguf` model |
| `whisper_language` | string | `'auto'` | Transcription language |
| `whisper_auto_insert` | boolean | true | Insert transcription at cursor |
| `voice_process_mode` | enum | `'none'` | Post-processing: none/format/summary |
| `whisper_server_port` | number | 8081 | Whisper server port |
| `llama_cli_path` | string | — | Path to llama-server binary |
| `llm_model_path` | string | — | Path to LLM `.gguf` model |
| `llama_server_port` | number | 8080 | LLM server port |
| `enable_chat` | boolean | false | Show Chat panel tab |
| `debug_mode` | boolean | false | Show Debug panel tab |
| `ai_provider` | string | `''` | Cloud AI provider (openai/anthropic/ollama) |
| `ai_model` | string | `'gpt-4o'` | Cloud AI model name |
| `ai_base_url` | string | OpenAI URL | Cloud AI base URL |

---

## Event Reference

| Event | Direction | Payload | Description |
|-------|-----------|---------|-------------|
| `whisper:stderr` | Rust → Frontend | string | whisper-server output |
| `llm:stderr` | Rust → Frontend | string | llama-server output |
| `llm:token` | Rust → Frontend | string | Streamed LLM token |
| `llm:done` | Rust → Frontend | — | Stream complete |
| `agent:tool_call` | Rust → Frontend | string | Human-readable tool invocation |
| `model-download-progress` | Rust → Frontend | `{model_id, downloaded_bytes, total_bytes, speed_bps, status}` | Download progress |
| `vault:note-created` | Rust → Frontend | path | File watcher: note created |
| `vault:note-updated` | Rust → Frontend | path | File watcher: note updated |
| `vault:note-deleted` | Rust → Frontend | path | File watcher: note deleted |
| `vault:note-renamed` | Rust → Frontend | `{old, new}` | File watcher: note renamed |

---

## Tauri Commands Reference

### Settings
| Command | Parameters | Returns |
|---------|-----------|---------|
| `get_settings` | — | `Settings` |
| `save_settings` | `settings: Settings` | — |
| `get_api_key` | `provider: string` | `string \| null` |
| `set_api_key` | `provider, key: string` | — |

### Vault
| Command | Parameters | Returns |
|---------|-----------|---------|
| `create_note` | `title, folder?, content?` | `Note` |
| `read_note` | `path` | `{ content, note }` |
| `update_note` | `path, content` | — |
| `delete_note` | `path` | — |
| `rename_note` | `path, newTitle` | `Note` |
| `move_note` | `path, newFolder` | — |
| `list_notes` | — | `Note[]` |
| `scan_vault` | — | `number` (count) |
| `get_backlinks` | `path` | `Note[]` |
| `create_folder` | `folderPath` | — |
| `list_folders` | — | `string[]` |
| `delete_folder` | `folderPath` | — |
| `move_folder` | `oldPath, newPath` | — |
| `import_image` | `sourcePath, folder?` | `Asset` |
| `list_assets` | — | `Asset[]` |
| `delete_asset` | `path` | — |
| `read_file_base64` | `path` | `string` |
| `list_trash` | — | `TrashItem[]` |
| `restore_trash_item` | `id, targetFolder` | — |
| `delete_trash_items` | `ids: string[]` | — |

### Search & Graph
| Command | Parameters | Returns |
|---------|-----------|---------|
| `search` | `query, limit?` | `SearchResult[]` |
| `get_graph` | — | `GraphData` |

### Voice & AI
| Command | Parameters | Returns |
|---------|-----------|---------|
| `transcribe_audio` | `pcmData: f32[], sampleRate` | `{ text }` |
| `stream_chat` | `messages, system?` | `string` |
| `process_with_llm` | `system, userContent` | `string` |
| `agent_chat` | `messages, system?` | `string` |
| `stop_whisper_server` | — | — |
| `stop_llama_server` | — | — |

### Import & Download
| Command | Parameters | Returns |
|---------|-----------|---------|
| `import_url` | `url` | `Note` |
| `get_models_dir` | — | `string` |
| `get_downloaded_models` | — | `ModelFile[]` |
| `start_model_download` | `url, modelId` | — |
| `cancel_model_download` | `modelId` | — |
| `delete_model_file` | `filename` | — |

---

## Security

- **Path traversal**: All vault paths validated against vault root; `..` segments blocked
- **SSRF protection**: `import_url` blocks localhost, loopback, and private IP ranges (10.x, 172.16.x, 192.168.x)
- **URL schemes**: Only `http://` and `https://` allowed for imports
- **API keys**: Stored in OS keyring, never in plaintext settings
- **SQL injection**: All queries use sqlx prepared statements
- **Content Security Policy**: `vault://` custom protocol for local assets; `unsafe-eval` for CodeMirror WASM
- **Agent path security**: Vault agent tool calls validate all paths are within vault directory

---

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (1.75+)
- [Node.js](https://nodejs.org/) (20+)
- [Tauri CLI](https://tauri.app/v2/guides/getting-started/prerequisites/)

### Run in Development

```bash
npm install
npm run tauri dev
```

### Build for Production

```bash
npm run tauri build
```

### Optional: Local AI Setup

1. Download [llama.cpp](https://github.com/ggerganov/llama.cpp) (provides `llama-server` and `whisper-server`)
2. Download a GGUF model (or use the in-app model manager)
3. Configure paths in **Settings → AI** and **Settings → Voice**
