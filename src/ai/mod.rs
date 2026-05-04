//! AI agent abstraction. Phase 1 ships only the trait + types so
//! the rest of the runtime can pass `Option<Arc<dyn AiAgent>>` around
//! without conditional compilation. Concrete `claude` / `gemini` /
//! `codex` backends arrive in Phase 3.

use async_trait::async_trait;
use camino::Utf8PathBuf;

use crate::error::Result;
pub use crate::manifest::AgentKind;

#[derive(Debug, Clone)]
pub struct AiRequest {
    pub system_prompt: String,
    pub user_prompt: String,
    /// Existing destination contents (None when creating).
    pub current: Option<String>,
    /// Newly-rendered template body for this destination.
    pub incoming: String,
    /// Optional template old-vs-new diff (for context to the agent).
    pub template_diff: Option<String>,
    /// Destination path (passed to the agent for context).
    pub dst: Utf8PathBuf,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AiResponse {
    /// Full replacement body. Mutually exclusive with `patch`.
    pub full_body: Option<String>,
    /// Unified diff to apply to `current`. Mutually exclusive with
    /// `full_body`.
    pub patch: Option<String>,
    /// Raw response body (for debugging / `--verbose`).
    pub raw: String,
    pub agent: AgentKind,
}

#[async_trait]
pub trait AiAgent: Send + Sync {
    fn kind(&self) -> AgentKind;
    async fn is_available(&self) -> bool;
    async fn run(&self, req: AiRequest) -> Result<AiResponse>;
}
