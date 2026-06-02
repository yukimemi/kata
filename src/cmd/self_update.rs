//! `kata self-update` — update the kata binary to the latest GitHub
//! release. Thin wrapper over [`crate::updater::run_self_update`].

use crate::error::Result;

/// Run the self-update flow.
///
/// - `yes` skips the confirmation prompt.
/// - `check` reports availability without installing.
/// - `non_interactive` bails (instead of prompting) when a prompt
///   would be required and `yes` is false.
pub async fn run(yes: bool, check: bool, non_interactive: bool) -> Result<()> {
    crate::updater::run_self_update(yes, check, non_interactive).await
}
