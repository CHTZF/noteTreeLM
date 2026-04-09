use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::sync::Mutex;
use serde_json::{json, Value};
use super::context::ContextBuffer;
use super::prompt::{templates, Locale};

use crate::db::SurrealDb;
use crate::service::types::{EmitEventFn, EmbedFn, AgentSession};
use crate::service::harness::engine::transaction::{TransactionState, AnswerChannel};
use crate::service::harness::engine::intent_classifier::{Intent, IntentClassifier};
use super::memory::working::WorkingMemory;
use super::engine::dispatcher::Dispatcher;
use super::context::{ContextPipeline, ContextBudget, ContextInput};
use super::memory::episodic::EpisodicMemory;
use super::observability::emitter::ObservabilityEmitter;
use super::governance::policy::build_need_confirm_fn;

/// Resolved runtime context for a single agent request.
///
/// Constructed once in the Axum handler (or scheduled task) via `ApiState::agent_runtime()`,
/// then passed through `run_agent` and all downstream helpers.
///
/// Contains both infrastructure fields (resolved from `ApiState`) and per-request execution
/// context (session, cancel flag, working memory, agent def, etc.).
/// Nothing in this struct back-references `ApiState`, breaking the circular dependency that
/// previously coupled every tool handler to the global service state.
#[derive(Clone)]
pub struct HarnessRequestRuntime {
    // ── Infrastructure (from ApiState) ────────────────────────────────────────
    pub db:               SurrealDb,
    /// Already-resolved LLM base URL (e.g. "http://127.0.0.1:8080").
    pub llm_url:          String,
    pub embedding_url:    Option<String>,
    pub vault_id:         String,
    pub account_id:       String,
    /// Active interactive agent sessions keyed by conv_id.
    pub agent_sessions:   Arc<Mutex<HashMap<String, AgentSession>>>,
    /// Intent centroid cache (confirm / cancel / interrupt embeddings).
    pub intent_centroids: Arc<Mutex<Option<(Vec<f32>, Vec<f32>, Vec<f32>)>>>,
    /// vault_id → vault_path in-process cache.
    pub vault_path_cache: Arc<std::sync::RwLock<HashMap<String, String>>>,

    // ── Per-request execution context ─────────────────────────────────────────
    /// Shared with `AgentSession::session_id` via `Arc` — guaranteed identical.
    pub session_id:     Arc<String>,
    /// Shared with `AgentSession::conv_id` via `Arc` — guaranteed identical.
    pub conv_id:        Arc<String>,
    /// Resolved vault filesystem path for this vault_id.
    pub vault_path:     String,
    /// Shared with `AgentSession::cancel` via `Arc`.
    pub cancel:         Arc<AtomicBool>,
    /// Shared with `AgentSession::answer_channel` via `Arc`.
    pub answer_channel: Arc<AnswerChannel>,
    pub(crate) working_memory: WorkingMemory,
    pub client:         reqwest::Client,
    /// Agent kind (e.g. "chat", "background", "sub", "scheduled").
    /// Use `is_background()` / `needs_frontend()` rather than comparing directly.
    pub kind:           String,
    pub streaming:      bool,
    /// Scoped knowledge source for tools like search_kb_pages.
    /// e.g. source_type="kb_session", source_id=<import_session_id>
    ///      source_type="vault",      source_id=<vault_id>
    pub source_type:    Option<String>,
    pub source_id:      Option<String>,
    /// UI language used to localise agent system messages.
    pub(crate) locale:  Locale,
    /// Raw agent definition from DB.
    pub agent_def:      Value,
    /// Pre-write file snapshots keyed by vault-relative path.
    /// Written by write-tool forward handlers; read by rollback handlers.
    pub(crate) write_snapshots: Arc<Mutex<HashMap<String, String>>>,
    /// Modification times (secs since epoch) at snapshot time.
    /// Used by write conflict detection.
    pub(crate) write_mtimes:    Arc<Mutex<HashMap<String, u64>>>,

    // ── Shared loop state (accessible from tool handlers) ────────────────────
    /// Message buffer + finish-signal for the current tool loop.
    /// All message management goes through this; working_memory lives separately
    /// because it is used by executor, emitter, and tool handlers as well.
    pub(crate) context: ContextBuffer,

    // ── Eagerly-built execution helpers ──────────────────────────────────────
    /// Session-stamped SSE emitter (built at construction time).
    pub(crate) emitter:    ObservabilityEmitter,
    /// Tool dispatcher wired to ALL_TOOL_DEFS. `None` in eval stubs.
    pub(crate) dispatcher: Option<Dispatcher>,
}

/// Output of the skill pre-pass + context pipeline step.
pub(crate) struct AgentContextResult {
    pub mem_facts_count:        usize,
    pub activated_skill_titles: Vec<String>,
    pub tool_names:             Vec<String>,
}

impl HarnessRequestRuntime {

    // ── Lifecycle helpers ─────────────────────────────────────────────────────

    /// Create a minimal stub runtime for eval test runs.
    /// All fields are empty/no-op — mock tool handlers ignore the env entirely.
    pub(crate) fn eval_stub() -> Self {
        let noop_emit: EmitEventFn = Arc::new(|_, _| {});
        let session_id             = Arc::new(String::new());
        let emitter = Self::build_emitter(&noop_emit, &session_id);
        HarnessRequestRuntime {
            db:               surrealdb::Surreal::init(),
            llm_url:          String::new(),
            embedding_url:    None,
            vault_id:         String::new(),
            account_id:       String::new(),
            agent_sessions:   Arc::new(Mutex::new(Default::default())),
            intent_centroids: Arc::new(Mutex::new(None)),
            vault_path_cache: Arc::new(std::sync::RwLock::new(Default::default())),
            session_id,
            conv_id:          Arc::new(String::new()),
            vault_path:       String::new(),
            cancel:           Arc::new(AtomicBool::new(false)),
            answer_channel:   Arc::new(AnswerChannel::new()),
            working_memory:   WorkingMemory::new(),
            client:           reqwest::Client::new(),
            kind:             String::new(),
            streaming:        false,
            source_type:      None,
            source_id:        None,
            locale:           Locale::default(),
            agent_def:        Value::Null,
            write_snapshots:  Arc::new(Mutex::new(Default::default())),
            write_mtimes:     Arc::new(Mutex::new(Default::default())),
            emitter,
            dispatcher: None,
            context:    ContextBuffer::new(),
        }
    }

    /// Dispatch a tool graph through the pre-built dispatcher.
    pub(crate) async fn dispatch(
        &self,
        tx:    Arc<super::engine::transaction::Transaction>,
        graph: super::engine::graph::ToolGraph,
    ) -> Result<Vec<Value>, String> {
        self.dispatcher
            .as_ref()
            .ok_or_else(|| "dispatch called on eval stub".to_string())?
            .run(Arc::new(self.clone()), tx, graph).await
    }

    /// Emit an SSE event via the session-stamped emitter.
    pub(crate) fn emit(&self, event: &str, payload: Value) {
        self.emitter.emit(event.to_string(), payload);
    }

    /// True for headless agent kinds that have no live frontend listener.
    pub fn is_background(&self) -> bool {
        matches!(self.kind.as_str(), "background" | "sub" | "scheduled")
    }

    /// True for agent kinds that require a live frontend (ask_user, progress tools).
    pub fn needs_frontend(&self) -> bool {
        !self.is_background()
    }

    /// Create a fresh runtime for a sub-agent invocation, sharing infrastructure
    /// (db, caches, emit_fn) from the parent but with a new session context.
    pub fn fork_for_sub_agent(&self, conv_id: String, agent_def: Value) -> HarnessRequestRuntime {
        let session_id = Arc::new(uuid::Uuid::new_v4().to_string());
        let kind       = "sub".to_string();
        let emitter    = Self::build_emitter(self.emitter.raw_emit_fn(), &session_id);
        let dispatcher = Some(Self::build_dispatcher(emitter.as_emit_fn(), &kind));
        HarnessRequestRuntime {
            db:               self.db.clone(),
            llm_url:          self.llm_url.clone(),
            embedding_url:    self.embedding_url.clone(),
            vault_id:         self.vault_id.clone(),
            account_id:       self.account_id.clone(),
            agent_sessions:   Arc::clone(&self.agent_sessions),
            intent_centroids: Arc::clone(&self.intent_centroids),
            vault_path_cache: Arc::clone(&self.vault_path_cache),
            vault_path:       self.vault_path.clone(),
            session_id,
            conv_id:        Arc::new(conv_id),
            cancel:         Arc::new(AtomicBool::new(false)),
            answer_channel: Arc::new(AnswerChannel::new()),
            working_memory: WorkingMemory::new(),
            client:         reqwest::Client::builder()
                                .timeout(Duration::from_secs(300))
                                .build()
                                .unwrap_or_else(|e| {
                                    tracing::warn!("[fork_for_sub_agent] reqwest client build failed: {}", e);
                                    reqwest::Client::new()
                                }),
            kind,
            streaming: false,
            source_type: self.source_type.clone(),
            source_id:   self.source_id.clone(),
            locale: Locale::default(),
            agent_def,
            write_snapshots: Arc::new(Mutex::new(HashMap::new())),
            write_mtimes:    Arc::new(Mutex::new(HashMap::new())),
            emitter,
            dispatcher,
            context: ContextBuffer::new(),
        }
    }

    /// Resolve vault filesystem path from vault_id.
    /// Checks the in-process cache first; falls back to DB query.
    pub async fn resolve_vault_path(&self, vault_id: &str) -> String {
        if vault_id.is_empty() { return String::new(); }
        if let Ok(cache) = self.vault_path_cache.read() {
            if let Some(path) = cache.get(vault_id) {
                return path.clone();
            }
        }
        #[derive(serde::Deserialize)]
        struct Row { path: String }
        let path: String = self.db
            .query("SELECT path FROM vaults WHERE vault_id = $vid LIMIT 1")
            .bind(("vid", vault_id.to_string()))
            .await
            .ok()
            .and_then(|mut r| r.take::<Vec<Row>>(0).ok())
            .and_then(|rows| rows.into_iter().next())
            .map(|r| r.path)
            .unwrap_or_default();
        if !path.is_empty() {
            if let Ok(mut cache) = self.vault_path_cache.write() {
                cache.insert(vault_id.to_string(), path.clone());
            }
        }
        path
    }

    /// Returns `true` if `input` was consumed by a suspended session (caller should return early).
    pub async fn try_resume(&self, input: &str) -> bool {
        let (pending_tx, answer_channel) = {
            let sessions = self.agent_sessions.lock().await;
            let s = sessions.get(self.conv_id.as_str());
            (
                s.and_then(|s| s.transaction.clone()),
                s.map(|s| Arc::clone(&s.answer_channel)),
            )
        };

        if let Some(ch) = answer_channel {
            if ch.try_resume(input.to_string()).await {
                return true;
            }
        }

        if let Some(pending) = pending_tx {
            if pending.state().await == TransactionState::Preparing {
                let embed_fn: EmbedFn = {
                    let client = self.client.clone();
                    let llm_url = self.llm_url.clone();
                    Arc::new(move |text: String| {
                        let client = client.clone();
                        let llm_url = llm_url.clone();
                        Box::pin(async move {
                            crate::embedding::embedder::embed_text_llm(&client, &llm_url, &text).await
                        })
                    })
                };
                let classifier = IntentClassifier::new();
                let intent = match IntentClassifier::compute_centroids_cached(
                    &self.intent_centroids,
                    &embed_fn,
                ).await {
                    Some((cc, ccl, ci)) => classifier.classify_with_centroids(input, &embed_fn, &cc, &ccl, &ci).await,
                    None => classifier.classify(input).await,
                };
                match intent {
                    Intent::Confirm => { let _ = pending.commit().await; }
                    Intent::Cancel | Intent::Interrupt => { let _ = pending.cancel().await; }
                    _ => {}
                }
                if matches!(intent, Intent::Confirm | Intent::Cancel | Intent::Interrupt) {
                    return true;
                }
            }
        }

        false
    }

    // ── Context buffer delegates ─────────────────────────────────────────────

    /// Initialize the message buffer (called once inside build_agent_context).
    async fn init_msgs(&self, messages: Vec<Value>) {
        self.context.init(messages).await;
    }

    /// Snapshot the message buffer.
    /// If `with_context_usage` is true, appends transient system messages for context
    /// pressure and stall detection — computed fresh each round, never stored.
    pub(crate) async fn get_context_msgs(&self, with_context_usage: bool) -> Vec<Value> {
        let mut snapshot = self.context.snapshot().await;
        if with_context_usage {
            // ── Context pressure ─────────────────────────────────────────────
            let bytes = snapshot.iter().map(ContextBuffer::msg_size).sum::<usize>();
            let pct   = bytes * 100 / ContextBuffer::CONTEXT_CRITICAL_BYTES;
            let budget = ContextBuffer::CONTEXT_CRITICAL_BYTES;
            let ctx_msg = if bytes >= ContextBuffer::CONTEXT_CRITICAL_BYTES {
                templates::context_critical_msg(pct, bytes, budget, self.locale)
            } else if bytes >= ContextBuffer::CONTEXT_WARN_BYTES {
                templates::context_warn_msg(pct, bytes, budget, self.locale)
            } else {
                templates::context_info_msg(pct, bytes, budget, self.locale)
            };
            snapshot.push(json!({ "role": "system", "content": ctx_msg }));

            // ── Stall detection ──────────────────────────────────────────────
            let repeats = self.working_memory.repeated_calls().await;
            if !repeats.is_empty() {
                let names: Vec<String> = repeats.iter()
                    .map(|(t, a)| if a.is_empty() { t.clone() } else { format!("{}({})", t, a) })
                    .collect();
                snapshot.push(json!({
                    "role": "system",
                    "content": templates::stall_warning(&names, self.locale),
                }));
            }
        }
        snapshot
    }

    /// Append one message to the buffer.
    pub(crate) async fn push_msg(&self, msg: Value) {
        self.context.push(msg).await;
    }

    /// Append messages with two safety nets:
    /// 1. Per-message cap via `ContextBuffer`.
    /// 2. Total budget: if current + incoming ≥ CONTEXT_CRITICAL_BYTES, auto-compress first.
    pub(crate) async fn extend_msgs_guarded(&self, new_msgs: Vec<Value>) {
        let current = self.context.byte_count().await;
        let incoming: usize = new_msgs.iter().map(ContextBuffer::msg_size).sum();

        if current + incoming >= ContextBuffer::CONTEXT_CRITICAL_BYTES {
            let calls = self.working_memory.snapshot_summary().await;
            let names: Vec<String> = calls.iter()
                .filter_map(|c| c["name"].as_str().map(String::from))
                .collect();
            let summary = templates::auto_compress_summary(calls.len(), &names, self.locale);
            self.context.compress(&summary, 4, &[], self.locale).await;
        }

        self.context.extend(new_msgs, self.locale).await;
    }

    // ── finish_answer delegates ──────────────────────────────────────────────

    /// Signal the tool loop to stop with `answer` as the final response.
    pub(crate) async fn set_finish_answer(&self, answer: String) {
        self.context.set_finish(answer).await;
    }

    /// Consume the finish signal (returns `Some` once, then `None`).
    pub(crate) async fn take_finish_answer(&self) -> Option<String> {
        self.context.take_finish().await
    }

    // ── write_snapshots / write_mtimes ───────────────────────────────────────

    /// Record the pre-write snapshot for `path` (only if not already recorded).
    pub(crate) async fn snapshot_if_absent(&self, path: String, content: String) {
        self.write_snapshots.lock().await.entry(path).or_insert(content);
    }

    /// Record the pre-write mtime for `path` (only if not already recorded).
    pub(crate) async fn record_mtime_if_absent(&self, path: String, mtime: u64) {
        self.write_mtimes.lock().await.entry(path).or_insert(mtime);
    }

    /// Record both snapshot and mtime (common pattern in write handlers).
    /// Each lock is acquired and released independently to avoid holding two locks at once.
    pub(crate) async fn snapshot_and_mtime_if_absent(&self, path: String, content: String, mtime: u64) {
        self.write_snapshots.lock().await.entry(path.clone()).or_insert(content);
        self.write_mtimes.lock().await.entry(path).or_insert(mtime);
    }

    /// Return the previously recorded snapshot for `path`, if any.
    pub(crate) async fn get_snapshot(&self, path: &str) -> Option<String> {
        self.write_snapshots.lock().await.get(path).cloned()
    }

    /// Compress the buffer — delegates to `ContextBuffer::compress`.
    pub(crate) async fn compress_msgs(&self, summary: &str, tail: usize, keep_ids: &[String]) -> usize {
        self.context.compress(summary, tail, keep_ids, self.locale).await
    }

    /// Remove this runtime's session from the active sessions map.
    pub async fn cleanup_session(&self) {
        let mut sessions = self.agent_sessions.lock().await;
        sessions.remove(self.conv_id.as_str());
    }

    // ── Agent execution builders ──────────────────────────────────────────────

    /// Build an `ObservabilityEmitter` that stamps every SSE payload with `session_id`.
    pub(crate) fn build_emitter(emit_fn: &EmitEventFn, session_id: &Arc<String>) -> ObservabilityEmitter {
        let base_emit    = Arc::clone(emit_fn);
        let session_id_c = Arc::clone(session_id);
        let raw_emit: EmitEventFn = Arc::new(move |event: String, mut payload: Value| {
            if let Value::Object(ref mut m) = payload {
                m.insert("session_id".to_string(), json!(session_id_c));
            }
            (base_emit)(event, payload);
        });
        ObservabilityEmitter::new(raw_emit)
    }

    /// Run the skill pre-pass + memory prefetch (Step 5) and context pipeline (Step 6) together.
    /// `system_prompt` is read from `self.agent_def["system_prompt"]`.
    pub(crate) async fn build_agent_context(
        &self,
        input:            &str,
        activity_context: Option<&str>,
        mut tool_names:   Vec<String>,
    ) -> AgentContextResult {
        let base_prompt = self.agent_def["system_prompt"].as_str().unwrap_or("").to_string();
        // Prepend user_image (free-text personal background) to the system prompt so the model
        // has location/language/occupation context without going through semantic search.
        let user_image = if !self.account_id.is_empty() {
            crate::service::harness::memory::user_image::load_user_image(&self.db, &self.account_id).await
        } else {
            String::new()
        };
        let system_prompt = if user_image.is_empty() {
            base_prompt
        } else {
            format!("【使用者背景】\n{}\n\n{}", user_image, base_prompt)
        };

        let pinned_skill_ids: Vec<String> = self.agent_def["skill_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
            .unwrap_or_default();
        let do_skill_pass = !self.vault_path.is_empty()
            && self.agent_def["use_skill_pass"].as_bool().unwrap_or(false);
        let do_memory_prefetch = self.needs_frontend() && !self.vault_id.is_empty() && !self.account_id.is_empty();
        let keywords: Vec<String> = input.split_whitespace()
            .filter(|w| w.chars().count() >= 2)
            .take(5)
            .map(String::from)
            .collect();

        // use_pinned: agent has explicitly pinned skills AND skill_pass is disabled
        let use_pinned = !self.agent_def["use_skill_pass"].as_bool().unwrap_or(false)
            && !pinned_skill_ids.is_empty();

        // Sticky skill: if a previous round already matched skills, re-use them without
        // re-running semantic search. Skills stay active until a round with no tool calls.
        let cached_skill = if do_skill_pass {
            self.working_memory.clone_active_skills().await
        } else {
            None
        };

        let (skill_result, prefetched_memory) = tokio::join!(
            async {
                if use_pinned {
                    Some(crate::service::harness::tools::skill_tools::load_skills_by_ids(
                        &self.db, &pinned_skill_ids,
                    ).await)
                } else if do_skill_pass {
                    let new_result = crate::service::helpers::run_skill_pass(
                        &self.client, &self.embedding_url, &self.db,
                        &self.vault_id, &self.account_id, input,
                    ).await;
                    if let Some(mut cached) = cached_skill {
                        // Merge: carry forward sticky skills and append any newly matched ones.
                        // This lets mid-conversation new requirements add skills without losing
                        // skills that were matched in earlier rounds.
                        for title in &new_result.skill_titles {
                            if !cached.skill_titles.contains(title) {
                                cached.skill_titles.push(title.clone());
                            }
                        }
                        for tool in &new_result.skill_tool_names {
                            if !cached.skill_tool_names.contains(tool) {
                                cached.skill_tool_names.push(tool.clone());
                            }
                        }
                        if !new_result.system_injection.is_empty() {
                            cached.system_injection = if cached.system_injection.is_empty() {
                                new_result.system_injection
                            } else {
                                format!("{}\n\n{}", cached.system_injection, new_result.system_injection)
                                    .chars().take(2000).collect()
                            };
                        }
                        Some(cached)
                    } else {
                        Some(new_result)
                    }
                } else {
                    None
                }
            },
            async {
                if do_memory_prefetch {
                    let facts = crate::service::helpers::vault_query_memory_with_limit(
                        &self.client, &self.embedding_url, &self.db,
                        &self.vault_id, &self.account_id, &keywords, 6,
                    ).await;
                    let fact_ids: Vec<String> = facts.iter()
                        .filter_map(|f| f["fact_id"].as_str().filter(|s| !s.is_empty()))
                        .map(|fid| format!("memory:{}:{}", self.vault_id, fid))
                        .collect();
                    Some((facts, fact_ids))
                } else {
                    None
                }
            }
        );

        let mut activated_skill_titles: Vec<String> = Vec::new();
        let system_injection = if let Some(skill) = skill_result {
            if skill.skill_titles.is_empty() {
                let discovery = super::tools::skill_tools::build_skill_discovery_injection(&self.db, &self.account_id).await;
                if !tool_names.contains(&"create_agent_skill".to_string()) {
                    tool_names.push("create_agent_skill".to_string());
                }
                discovery
            } else {
                tracing::info!("[skill_pass] activated {} skills: {:?}", skill.skill_titles.len(), skill.skill_titles);
                if self.streaming {
                    self.emit("agent:skills_activated", json!({
                        "titles": skill.skill_titles,
                        "source": "pre_pass",
                    }));
                }
                activated_skill_titles = skill.skill_titles.clone();
                // Persist skills in WorkingMemory so subsequent ReAct rounds can re-use them
                // without re-running semantic search. Cleared when a round produces no tool calls.
                if !self.working_memory.has_active_skills().await {
                    self.working_memory.set_active_skills(skill.clone()).await;
                }
                for t in skill.skill_tool_names {
                    if !tool_names.contains(&t) { tool_names.push(t); }
                }
                skill.system_injection
            }
        } else {
            String::new()
        };

        let (mem_facts, mem_fact_ids) = prefetched_memory.unwrap_or_default();
        let pipeline = ContextPipeline::new(ContextBudget::default());
        let built = pipeline.build(
            ContextInput {
                db:               &self.db,
                conv_id:          &self.conv_id,
                vault_id:         &self.vault_id,
                user_input:       input,
                system_prompt:    &system_prompt,
                skill_injection:  &system_injection,
                activity_context,
                memory_facts:     &mem_facts,
                is_chat:          self.streaming,
                locale:           self.locale,
            },
            &self.client,
            &self.llm_url,
        ).await;
        if built.was_trimmed {
            tracing::debug!(
                "[context] history trimmed: before={}chars system={}chars",
                built.history_chars_before_trim,
                built.system_chars_used,
            );
        }
        if self.streaming && !mem_fact_ids.is_empty() {
            self.emit("memory:prefetched", json!({
                "node_ids": mem_fact_ids,
                "source": "chat",
            }));
        }

        self.init_msgs(built.messages).await;
        AgentContextResult {
            mem_facts_count: mem_facts.len(),
            activated_skill_titles,
            tool_names,
        }
    }

    /// Wire up `need_confirm_fn` + dispatcher registry and return a `Dispatcher`.
    pub(crate) fn build_dispatcher(emit_fn: EmitEventFn, kind: &str) -> Dispatcher {
        use super::engine::tool_registry::ToolRegistry;
        use super::governance::policy::assert_guard_coverage;
        use super::tool_def::ALL_TOOL_DEFS;
        use crate::service::types::{HandlerFn, Tool};

        assert_guard_coverage();
        let mut registry = ToolRegistry::new();
        for def in ALL_TOOL_DEFS {
            let def = *def;
            registry.register(def.name.to_string(), Tool {
                execute:  Arc::new(def.handler) as HandlerFn,
                rollback: def.rollback.map(|f| Arc::new(f) as HandlerFn),
                guard:    def.guard,
            });
        }
        Dispatcher::new(Arc::new(registry), emit_fn, build_need_confirm_fn(kind))
    }

    /// Emit final SSE events and persist conversation + trace to DB.
    pub(crate) async fn post_process(
        &self,
        msgs:            &[Value],
        full_response:   &str,
        input:           &str,
        mem_facts_count: usize,
    ) {
        if self.streaming {
            self.emit("llm:done", json!(full_response));
            if !full_response.is_empty() && crate::service::helpers::detect_response_framework(full_response) {
                self.emit("agent:skill_suggestion", json!({
                    "query": input,
                    "response_preview": full_response.chars().take(200).collect::<String>(),
                }));
            }
        }

        {
            let mut to_save: Vec<Value> = msgs.iter()
                .filter(|m| matches!(m["role"].as_str(), Some("user") | Some("assistant")))
                .filter(|m| m["tool_calls"].is_null())
                .filter(|m| !m["content"].as_str().unwrap_or("").is_empty())
                .cloned()
                .collect();
            if !full_response.is_empty() {
                to_save.push(json!({ "role": "assistant", "content": full_response }));
            }
            let episodic = EpisodicMemory::new(self.db.clone());
            episodic.append(&self.conv_id, &to_save).await;
            episodic.set_title_if_blank(&self.conv_id, &to_save).await;
        }

        let trace = self.emitter.finish(&self.working_memory, &self.conv_id, mem_facts_count).await;
        tracing::debug!(
            "[session:trace] session={} rounds={} tools={} blocked={} tool_ms={}ms",
            self.session_id, trace.round_count, trace.total_calls(), trace.blocked_calls(), trace.total_tool_ms(),
        );
        super::observability::trace_store::save_session_trace(
            &self.db, &self.vault_id, &self.account_id, &trace,
        ).await;
    }

}
