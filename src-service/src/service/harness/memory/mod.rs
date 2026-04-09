/// Three-layer memory architecture for interactive agent sessions.
///
/// | Layer           | Scope       | Backend                        | Eviction         |
/// |-----------------|-------------|--------------------------------|------------------|
/// | WorkingMemory   | Session     | Arc<Mutex<HashMap>> in-process | clear() on end   |
/// | EpisodicMemory  | Conversation| SurrealDB conversations table  | trim() by count  |
/// | SemanticMemory  | Vault       | SurrealDB memory_facts + embed | prune_before() ts|
pub(crate) mod working;
pub(crate) mod episodic;
pub(crate) mod semantic;
pub(crate) mod user_image;
