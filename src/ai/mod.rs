//! AI agent abstraction.
//!
//! - **Trait + DTOs** (`AiAgent`, `AiRequest`, `AiResponse`) are
//!   the contract `modes/ai.rs` calls into.
//! - **`process` submodule** is the low-level subprocess plumbing
//!   shared by every concrete backend: `Backend`, `resolve_cli`,
//!   `invoke_chat` (one chat turn), `run_handoff` (escape hatch
//!   to interactive mode).
//!
//! Phase 3-a shipped the trait + the subprocess plumbing. Phase
//! 3-b1 layers the concrete `ChatAgent` (one struct, picks its
//! backend at construction time), the auto-resolver, the prompt
//! builder, and the body extractor on top. The chezmoi-style
//! per-file UI lands with Phase 3-b3.

pub mod agent;
pub mod process;

use async_trait::async_trait;
use camino::Utf8PathBuf;

use crate::error::Result;
pub use crate::manifest::AgentKind;
pub use agent::{
    ChatAgent, DEFAULT_SYSTEM_PROMPT, agent_for_kind, ensure_agent_available, extract_body,
    format_prompt, resolve_backend,
};
pub use process::{
    Backend, ResolvedCli, ensure_cli_installed, invoke_chat, resolve_cli, run_handoff,
};

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
    // No per-request `timeout_secs`: the chat-turn timeout is owned
    // by `process::invoke_chat` and configured globally via
    // `KATA_AI_TIMEOUT_SECS` (default 300s). A field here would have
    // to be honoured by every concrete `AiAgent` impl, and
    // `ChatAgent::run` would silently ignore it. Re-introduce the
    // override the day a backend genuinely needs per-call timeouts.
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
