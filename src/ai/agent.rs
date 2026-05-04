//! Concrete `AiAgent` implementation that drives any
//! `process::Backend` (claude / gemini / codex) the same way: build
//! a structured prompt → pipe it to the CLI → extract the body
//! the agent returned. One struct, one dispatch table, no cargo
//! cult `ClaudeAgent` / `GeminiAgent` / `CodexAgent` triplet — they
//! would only differ in the literal `Backend` value.
//!
//! The structured prompt uses XML-style tags (`<kata:current>`,
//! `<kata:incoming>`, `<kata:body>`) borrowed from rvpm's pattern
//! so the agent's reply is robustly extractable even when it wraps
//! its output in code fences or adds a preamble.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::manifest::AgentKind;

use super::process::{Backend, invoke_chat};
use super::{AiAgent, AiRequest, AiResponse};

/// Single concrete `AiAgent`. Picks its underlying CLI based on
/// `Backend` (set at construction time via `for_kind` /
/// `agent_for_kind`).
pub struct ChatAgent {
    backend: Backend,
}

impl ChatAgent {
    pub fn new(backend: Backend) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }
}

#[async_trait]
impl AiAgent for ChatAgent {
    fn kind(&self) -> AgentKind {
        match self.backend {
            Backend::Claude => AgentKind::Claude,
            Backend::Gemini => AgentKind::Gemini,
            Backend::Codex => AgentKind::Codex,
        }
    }

    async fn is_available(&self) -> bool {
        self.backend.is_available()
    }

    async fn run(&self, req: AiRequest) -> Result<AiResponse> {
        let prompt = format_prompt(&req);
        let raw = invoke_chat(self.backend, &prompt).await?;
        let body = extract_body(&raw);
        Ok(AiResponse {
            full_body: body,
            patch: None,
            raw,
            agent: self.kind(),
        })
    }
}

/// Default system prompt. Kept short and stable so it survives
/// agent retries — the per-file user prompt does the heavy lifting.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are kata, a multi-project template applier. The user is asking you to merge \
     a freshly-rendered template body into an existing destination file. Reply with \
     the merged final file body wrapped in a single <kata:body>...</kata:body> XML \
     tag. Do not add code fences, explanations, or any text outside that tag.";

/// Build the actual stdin payload for one chat turn. Lays out the
/// system prompt, the manifest-author prompt, and the
/// `<kata:current>` / `<kata:incoming>` / `<kata:dst>` context
/// blocks the agent is expected to merge.
pub fn format_prompt(req: &AiRequest) -> String {
    // Pre-size to "everything we'll write" + a small headroom for
    // the static tag wrappers / closing instruction. Keeps large
    // CLAUDE.md / ROADMAP.md merges from triggering several
    // reallocations.
    let mut buf = String::with_capacity(
        req.system_prompt.len()
            + req.user_prompt.len()
            + req.dst.as_str().len()
            + req.current.as_ref().map(|s| s.len()).unwrap_or(0)
            + req.incoming.len()
            + req.template_diff.as_ref().map(|s| s.len()).unwrap_or(0)
            + 256,
    );
    if !req.system_prompt.is_empty() {
        buf.push_str(&req.system_prompt);
        buf.push_str("\n\n");
    }
    if !req.user_prompt.is_empty() {
        buf.push_str(&req.user_prompt);
        buf.push_str("\n\n");
    }
    buf.push_str("<kata:dst>");
    buf.push_str(req.dst.as_str());
    buf.push_str("</kata:dst>\n\n");
    if let Some(c) = &req.current {
        buf.push_str("<kata:current>\n");
        buf.push_str(c);
        if !c.ends_with('\n') {
            buf.push('\n');
        }
        buf.push_str("</kata:current>\n\n");
    }
    buf.push_str("<kata:incoming>\n");
    buf.push_str(&req.incoming);
    if !req.incoming.ends_with('\n') {
        buf.push('\n');
    }
    buf.push_str("</kata:incoming>\n\n");
    if let Some(d) = &req.template_diff {
        buf.push_str("<kata:template_diff>\n");
        buf.push_str(d);
        if !d.ends_with('\n') {
            buf.push('\n');
        }
        buf.push_str("</kata:template_diff>\n\n");
    }
    buf.push_str("Reply with the merged final file body for <kata:dst> wrapped in a single <kata:body>...</kata:body> tag.\n");
    buf
}

/// Pull the merged body out of the agent's reply.
///
/// We look for the **last** `<kata:body>...</kata:body>` block —
/// some CLIs (notably gemini) like to echo prior tags in their
/// preamble before settling on the actual answer. If the agent
/// failed to honour the tag at all we return `None` rather than
/// silently writing whatever the agent said; the mode layer will
/// surface a clear error.
pub fn extract_body(raw: &str) -> Option<String> {
    const OPEN: &str = "<kata:body>";
    const CLOSE: &str = "</kata:body>";
    let open_off = raw.rfind(OPEN)? + OPEN.len();
    let rest = raw.get(open_off..)?;
    let close_off = rest.find(CLOSE)?;
    let body = &rest[..close_off];
    // Strip exactly one leading and one trailing newline so block
    // formatting reads naturally; preserve internal whitespace.
    // Handle CRLF too — Windows agents and some models emit `\r\n`
    // and a single-byte strip would otherwise leave a stray `\r`.
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))
        .unwrap_or(body);
    let body = body
        .strip_suffix("\r\n")
        .or_else(|| body.strip_suffix('\n'))
        .unwrap_or(body);
    Some(body.to_string())
}

/// Map the manifest-side `AgentKind` to a concrete `Backend`,
/// resolving `Auto` against the user's PATH. CLAUDE.md fixes the
/// fallback order at **claude > codex > gemini**.
pub fn resolve_backend(kind: AgentKind) -> Option<Backend> {
    match kind {
        AgentKind::Claude => Some(Backend::Claude),
        AgentKind::Gemini => Some(Backend::Gemini),
        AgentKind::Codex => Some(Backend::Codex),
        AgentKind::Auto => {
            for b in [Backend::Claude, Backend::Codex, Backend::Gemini] {
                if b.is_available() {
                    return Some(b);
                }
            }
            None
        }
    }
}

/// Build a ready-to-use `AiAgent` from a manifest `AgentKind`.
/// Returns `None` when no usable backend resolves on PATH — both
/// for `Auto` (the resolver walks claude > codex > gemini and
/// finds nothing) and for an explicit `Claude` / `Gemini` /
/// `Codex` whose CLI isn't installed. Returning `None` here lets
/// the mode layer report a clean `Skipped` outcome with a clear
/// hint instead of failing the whole apply run on a missing
/// optional dependency.
pub fn agent_for_kind(kind: AgentKind) -> Option<Arc<dyn AiAgent>> {
    let backend = resolve_backend(kind)?;
    if !backend.is_available() {
        return None;
    }
    Some(Arc::new(ChatAgent::new(backend)))
}

/// Sanity-check helper that surfaces a clear error when the
/// configured agent isn't usable. Used by `kata doctor` and the
/// apply path before scheduling AI work.
pub fn ensure_agent_available(kind: AgentKind) -> Result<Backend> {
    resolve_backend(kind).ok_or_else(|| {
        Error::Other(anyhow::anyhow!(
            "no AI CLI on PATH (`agent = {kind:?}` requested). \
             Install one of claude / codex / gemini, or pass `--no-ai` to skip AI files.",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn req(current: Option<&str>, incoming: &str, user: &str) -> AiRequest {
        AiRequest {
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            user_prompt: user.to_string(),
            current: current.map(|s| s.to_string()),
            incoming: incoming.to_string(),
            template_diff: None,
            dst: Utf8PathBuf::from("CLAUDE.md"),
            timeout_secs: 300,
        }
    }

    #[test]
    fn format_prompt_includes_dst_incoming_and_no_current_when_creating() {
        let p = format_prompt(&req(None, "BODY\n", "merge for CLAUDE.md"));
        assert!(p.contains("<kata:dst>CLAUDE.md</kata:dst>"));
        assert!(p.contains("<kata:incoming>\nBODY\n</kata:incoming>"));
        assert!(!p.contains("<kata:current>"));
        assert!(p.contains("<kata:body>"), "must instruct on response tag");
    }

    #[test]
    fn format_prompt_wraps_existing_body_in_current_block() {
        let p = format_prompt(&req(Some("OLD"), "NEW", "merge"));
        assert!(p.contains("<kata:current>\nOLD\n</kata:current>"));
        assert!(p.contains("<kata:incoming>\nNEW\n</kata:incoming>"));
    }

    #[test]
    fn format_prompt_passes_template_diff_when_provided() {
        let mut r = req(Some("OLD"), "NEW", "merge");
        r.template_diff = Some("--- a\n+++ b\n@@ ...".to_string());
        let p = format_prompt(&r);
        assert!(p.contains("<kata:template_diff>"));
        assert!(p.contains("@@ ..."));
    }

    #[test]
    fn format_prompt_normalises_missing_trailing_newlines() {
        let p = format_prompt(&req(Some("no-newline"), "still-no-newline", "merge"));
        assert!(p.contains("\nno-newline\n</kata:current>"));
        assert!(p.contains("\nstill-no-newline\n</kata:incoming>"));
    }

    #[test]
    fn extract_body_picks_last_kata_body_block() {
        // The first occurrence is in the agent's preamble where it
        // *describes* the tag; the last is the real answer.
        let raw = "I will use a <kata:body>...</kata:body> tag.\n\
                   <kata:body>\nFINAL CONTENT\n</kata:body>\n";
        assert_eq!(extract_body(raw).as_deref(), Some("FINAL CONTENT"));
    }

    #[test]
    fn extract_body_returns_none_when_tag_missing() {
        assert_eq!(extract_body("just plain prose, no tags"), None);
    }

    #[test]
    fn extract_body_handles_empty_block() {
        let raw = "<kata:body></kata:body>";
        assert_eq!(extract_body(raw).as_deref(), Some(""));
    }

    #[test]
    fn extract_body_preserves_internal_blank_lines() {
        let raw = "<kata:body>\nline1\n\nline3\n</kata:body>";
        assert_eq!(
            extract_body(raw).as_deref(),
            Some("line1\n\nline3"),
            "internal whitespace must round-trip",
        );
    }

    #[test]
    fn extract_body_strips_crlf_wrappers() {
        // Windows agents and some models wrap the tag in CRLF —
        // a single-byte strip would leave a stray `\r` inside
        // `body`, which is invisible but corrupts the file.
        let raw = "<kata:body>\r\nFINAL CONTENT\r\n</kata:body>";
        assert_eq!(extract_body(raw).as_deref(), Some("FINAL CONTENT"));
    }

    #[test]
    fn resolve_backend_maps_explicit_kinds_directly() {
        assert_eq!(resolve_backend(AgentKind::Claude), Some(Backend::Claude));
        assert_eq!(resolve_backend(AgentKind::Gemini), Some(Backend::Gemini));
        assert_eq!(resolve_backend(AgentKind::Codex), Some(Backend::Codex));
    }

    #[test]
    fn resolve_backend_auto_returns_none_when_no_cli_is_installed() {
        // Skip the test if any CLI happens to be on PATH on the
        // dev box — the assertion is about the empty-PATH branch.
        if Backend::Claude.is_available()
            || Backend::Codex.is_available()
            || Backend::Gemini.is_available()
        {
            return;
        }
        assert!(resolve_backend(AgentKind::Auto).is_none());
    }

    #[test]
    fn chat_agent_kind_round_trips_through_backend() {
        for (kind, backend) in [
            (AgentKind::Claude, Backend::Claude),
            (AgentKind::Gemini, Backend::Gemini),
            (AgentKind::Codex, Backend::Codex),
        ] {
            let a = ChatAgent::new(backend);
            assert_eq!(a.kind(), kind);
        }
    }
}
