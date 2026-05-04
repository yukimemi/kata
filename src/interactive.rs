//! Inquire-based interactive prompts. Phase 1 ships only the var
//! prompter; the AI `[a]ccept / [e]dit / [s]kip / [d]efer` selector
//! lands in Phase 3.

use crate::error::{Error, Result};
use crate::manifest::VarSpec;

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
