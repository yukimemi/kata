//! Inquire-based interactive prompts. Hosts the var prompter and
//! the chezmoi-style `[a]ccept / [e]dit / [c]hat / [h]andoff /
//! [s]kip / [d]efer` AI decision dialog used by `modes/ai.rs`.

use crate::error::{Error, Result};
use crate::manifest::VarSpec;

/// What the user picked for an AI-produced body. The shape mirrors
/// chezmoi's `[a]ccept / [e]dit / [s]kip / [d]efer` plus two
/// kata-specific arms: `[c]hat` ("ask the AI again with this
/// hint") and `[h]andoff` ("escape hatch — drop me into the agent
/// CLI and stop re-importing").
#[derive(Debug, Clone)]
pub enum AiDecision {
    /// Write the AI's body to disk.
    Accept,
    /// Open the AI body in `$EDITOR` and write the user's edited
    /// version. No further AI calls.
    Edit,
    /// Hand the AI a one-line refinement and re-run. The string is
    /// the user's instruction, e.g. "make it shorter".
    Chat(String),
    /// Spawn the agent CLI interactively. kata stops re-importing —
    /// the agent's own Edit / Write tools take over from here.
    Handoff,
    /// Skip this round; do not record a `defer`.
    Skip,
    /// Skip this round but ask again on the next apply.
    Defer,
}

const CHOICES: [&str; 6] = [
    "[a]ccept   write the AI body to disk",
    "[e]dit     open the body in $EDITOR before writing",
    "[c]hat     give the AI a one-line refinement and try again",
    "[h]andoff  open the agent CLI interactively (kata stops re-importing)",
    "[s]kip     skip this round (do not retry next apply)",
    "[d]efer    skip this round but ask again next apply",
];

/// Run the chezmoi-style decision prompt for one `how = "ai"`
/// file. Caller has already shown the diff, so the prompt itself
/// stays terse.
///
/// `Esc` (which `inquire` reports as `OperationCanceled`) collapses
/// to `Defer` to match the help message — bailing out of a single
/// AI prompt should not abort the whole apply run. Ctrl-C
/// (`OperationInterrupted`) is still a hard cancel because the user
/// is asking the entire session to stop.
pub fn prompt_ai_decision(dst: &str) -> Result<AiDecision> {
    let label = format!("AI proposal for {dst}:");
    let pick = match inquire::Select::new(&label, CHOICES.to_vec())
        .with_help_message("\u{2191}\u{2193} to move, Enter to confirm, Esc to defer")
        .prompt()
    {
        Ok(p) => p,
        Err(inquire::InquireError::OperationCanceled) => return Ok(AiDecision::Defer),
        Err(e) => return Err(map_inquire_err(e)),
    };

    let starts_with = |k: char| pick.starts_with(&format!("[{k}]"));
    if starts_with('a') {
        return Ok(AiDecision::Accept);
    }
    if starts_with('e') {
        return Ok(AiDecision::Edit);
    }
    if starts_with('h') {
        return Ok(AiDecision::Handoff);
    }
    if starts_with('s') {
        return Ok(AiDecision::Skip);
    }
    if starts_with('d') {
        return Ok(AiDecision::Defer);
    }
    if starts_with('c') {
        let instr = match inquire::Text::new("Refinement instruction (1 line):")
            .with_help_message(
                "e.g. \"keep my custom Section X\" / \"shorter\" / \"add a note about ROADMAP\"",
            )
            .prompt()
        {
            Ok(s) => s,
            // Esc inside the chat-instruction prompt also rolls
            // back to Defer rather than aborting the run.
            Err(inquire::InquireError::OperationCanceled) => return Ok(AiDecision::Defer),
            Err(e) => return Err(map_inquire_err(e)),
        };
        return Ok(AiDecision::Chat(instr));
    }
    // Should be unreachable given the six-element CHOICES list,
    // but be defensive — prefer Defer over panicking on an
    // unexpected option string.
    Ok(AiDecision::Defer)
}

/// Prompt the user for a single variable's value, honouring the spec
/// (`choices` → Select, `secret` → Password, otherwise a text
/// input with optional default).
pub fn prompt_var(name: &str, spec: &VarSpec) -> Result<toml::Value> {
    let label = spec.prompt.as_deref().unwrap_or(name);

    if let Some(choices) = &spec.choices {
        let ans = inquire::Select::new(label, choices.clone())
            .prompt()
            .map_err(map_inquire_err)?;
        return Ok(toml::Value::String(ans));
    }

    if spec.secret {
        let ans = inquire::Password::new(label)
            .without_confirmation()
            .prompt()
            .map_err(map_inquire_err)?;
        return Ok(toml::Value::String(ans));
    }

    let default_str = spec.default.as_ref().and_then(|v| match v {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(n) => Some(n.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        _ => None,
    });
    let mut text = inquire::Text::new(label);
    if let Some(d) = &default_str {
        text = text.with_default(d);
    }
    let ans = text.prompt().map_err(map_inquire_err)?;
    Ok(toml::Value::String(ans))
}

fn map_inquire_err(e: inquire::InquireError) -> Error {
    use inquire::InquireError as IE;
    match e {
        IE::OperationCanceled | IE::OperationInterrupted => Error::Cancelled,
        other => Error::Other(anyhow::anyhow!("prompt: {other}")),
    }
}
