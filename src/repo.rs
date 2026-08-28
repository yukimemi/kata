//! `[repo]` — GitHub-side project state, checked and converged the way
//! files are.
//!
//! Some project state is not a file. `delete_branch_on_merge` and the
//! Actions secrets live on GitHub, so no template layer could own them
//! and every new repository needed the same manual clicking. Declaring
//! that state here buys the three moments the file pipeline already has:
//! set it up at `kata init`, find the drift with `kata status`, converge
//! it with `kata apply`.
//!
//! It is deliberately *not* an `ApplyMode`. `ActionContext` is built
//! around `src_abs` / `dst_abs` / `rendered_body`, and a repository
//! setting has none of those — an entry here would have to invent a
//! source path that does not exist. `how = "script"` was the other
//! candidate and fails for a sharper reason: a script has no readable
//! current state, so `plan` can only ever say "would run", and a
//! cross-repository check that says that about every project forever is
//! not a check.
//!
//! Everything reachable from `plan_settings` / `plan_secret_names` is
//! pure, so the interesting logic is unit-tested without a network.

use async_trait::async_trait;
use camino::Utf8Path;
use serde_json::Value as Json;
use tokio::process::Command;

use crate::error::{Error, Result};

/// `owner/repo` on github.com.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slug {
    pub owner: String,
    pub repo: String,
}

impl std::fmt::Display for Slug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

/// One field of `[repo.settings]` that does not match the live repository.
///
/// `Unknown` is how a pass-through table recovers most of the schema
/// validation it gave up: drift detection already holds the `GET`
/// response, so a desired key the API never returns is almost certainly a
/// typo and is worth saying so about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingChange {
    Update {
        key: String,
        from: String,
        to: String,
    },
    Unknown {
        key: String,
    },
}

/// An Actions secret that is declared but not present on the repository.
///
/// There is no `Update` arm: a secret's value cannot be read back, so
/// drift is existence rather than equality and a rotation is invisible to
/// kata. A deliberate limit, not an oversight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretChange {
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RepoPlan {
    pub settings: Vec<SettingChange>,
    pub secrets: Vec<SecretChange>,
}

impl RepoPlan {
    pub fn is_empty(&self) -> bool {
        self.settings.is_empty() && self.secrets.is_empty()
    }

    /// Is there anything to converge?
    ///
    /// A plan holding only warnings is not work: `execute` PATCHes only
    /// `Update` entries, so an `Unknown` changes nothing on GitHub.
    pub fn has_work(&self) -> bool {
        !self.secrets.is_empty()
            || self
                .settings
                .iter()
                .any(|c| matches!(c, SettingChange::Update { .. }))
    }

    /// One line per change `execute` will actually make.
    ///
    /// Kept apart from `warning_lines` because a caller that mixes the
    /// two cannot tell them apart afterwards, and would go on to report a
    /// typo as a write.
    ///
    /// Nothing here can carry a secret value: `plan_secret_names` is only
    /// ever handed names, so no value is in scope to leak into a diff, a
    /// `--commit` message, or a terminal.
    pub fn work_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for change in &self.settings {
            if let SettingChange::Update { key, from, to } = change {
                out.push(format!("{key}: {from} → {to}"));
            }
        }
        for secret in &self.secrets {
            out.push(format!("{}: absent → present", secret.name));
        }
        out
    }

    /// Lines worth showing that are not work — currently keys the API
    /// never returned, which are almost always typos.
    pub fn warning_lines(&self) -> Vec<String> {
        self.settings
            .iter()
            .filter_map(|change| match change {
                SettingChange::Unknown { key } => Some(format!(
                    "warn: `{key}` is not a field the API returns — typo?"
                )),
                SettingChange::Update { .. } => None,
            })
            .collect()
    }
}

/// Compare the desired `[repo.settings]` table against a
/// `GET repos/{owner}/{repo}` response. Fields that already match are
/// left out; only work and warnings come back.
pub fn plan_settings(desired: &toml::Table, actual: &Json) -> Vec<SettingChange> {
    let mut out = Vec::new();
    for (key, want) in desired {
        match actual.get(key) {
            None => out.push(SettingChange::Unknown { key: key.clone() }),
            Some(have) if json_matches_toml(have, want) => {}
            Some(have) => out.push(SettingChange::Update {
                key: key.clone(),
                from: show_json(have),
                to: show_toml(want),
            }),
        }
    }
    out
}

/// Which declared secrets are missing from the repository.
///
/// Takes names, never `RepoSecretSpec`, so a value cannot reach the
/// planning path even by accident.
pub fn plan_secret_names(desired: &[String], existing: &[String]) -> Vec<SecretChange> {
    desired
        .iter()
        .filter(|name| !existing.iter().any(|e| e == *name))
        .map(|name| SecretChange { name: name.clone() })
        .collect()
}

/// Parse `owner/repo` out of a git remote URL.
///
/// Returns `None` for any host that is not github.com — a PJ on another
/// forge is not-applicable rather than broken, and must skip cleanly.
pub fn parse_slug(url: &str) -> Option<Slug> {
    let rest = if let Some(r) = url.strip_prefix("git@") {
        // scp-style: git@github.com:owner/repo.git — the colon is the
        // path separator here, so normalise it before splitting.
        r.replacen(':', "/", 1)
    } else {
        url.strip_prefix("ssh://git@")
            .or_else(|| url.strip_prefix("https://"))
            .or_else(|| url.strip_prefix("http://"))?
            .to_string()
    };

    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);

    let mut parts = rest.split('/');
    let host = parts.next()?;
    // Tolerate an explicit port on an ssh:// URL.
    let host = host.split(':').next()?;
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(Slug {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

/// Whether this machine can talk to GitHub at all.
///
/// Not having `gh`, or having it unauthenticated, is a fact about the
/// environment rather than drift on the repository — the same category
/// as a PJ whose `origin` is not GitHub. Treating it as a failure would
/// break the one place kata runs unattended: the daily `kata-apply`
/// workflow shells out on a runner where `gh` is installed but no
/// `GH_TOKEN` is exported, and `kata apply` exits non-zero as soon as
/// any action fails. That failure lands *before* the step that opens
/// the PR, so a template shipping `[repo]` could not deliver the
/// workflow fix for its own breakage. Skipping is what keeps that from
/// being a deadlock.
///
/// The cost is that a genuinely broken local `gh` login is skipped
/// rather than shouted about — but the row still prints its reason, so
/// it is quiet, not invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    /// `gh` is not on PATH.
    NoGh,
    /// `gh` is installed but has no usable login.
    NotAuthenticated,
}

impl Readiness {
    /// The parenthetical shown on a skipped row.
    pub fn reason(&self) -> &'static str {
        match self {
            Readiness::Ready => "ready",
            Readiness::NoGh => "gh is not installed",
            Readiness::NotAuthenticated => "gh is not authenticated",
        }
    }
}

/// Probed at most once per command run.
///
/// Whether `gh` exists and holds a github.com login is process-global
/// and cannot change under a single invocation, while `readiness()` is
/// called once per `[repo]`-bearing PJ — and `status --all --repo` fans
/// that out across the whole registry. Without this, a registry of
/// twenty pays forty `gh` spawns, one of which validates a credential
/// over the network, to answer the same question twenty times.
static READINESS: tokio::sync::OnceCell<Readiness> = tokio::sync::OnceCell::const_new();

/// Is `gh` present, and logged in to github.com?
///
/// `gh auth status` is the check rather than a trial API call, because a
/// failing `gh api` cannot distinguish "no credentials" from "this
/// repository does not exist" or "the network is down", and only the
/// first of those should be quiet.
pub async fn readiness() -> Readiness {
    READINESS.get_or_init(probe_readiness).await.clone()
}

async fn probe_readiness() -> Readiness {
    let installed = Command::new("gh")
        .arg("--version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !installed {
        return Readiness::NoGh;
    }
    // Scoped to github.com deliberately. Bare `gh auth status` reports
    // on every configured host, and everything downstream here talks to
    // github.com and nothing else — so an unscoped check answers a
    // different question than the one being asked, in both directions:
    // a login to an enterprise host only would pass it and then fail at
    // `gh api` (reopening the very deadlock this skip exists to
    // prevent), and a stale credential for some unrelated host would
    // fail it while github.com is perfectly usable.
    let authed = Command::new("gh")
        .args(["auth", "status", "--hostname", "github.com"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if authed {
        Readiness::Ready
    } else {
        Readiness::NotAuthenticated
    }
}

/// The GitHub slug for a project checkout, or `None` when it has no
/// github.com `origin`.
pub async fn slug_of(pj_root: &Utf8Path) -> Option<Slug> {
    if let Some(url) = git_origin(pj_root).await {
        return parse_slug(&url);
    }
    parse_slug(&jj_origin(pj_root).await?)
}

/// `origin` as git reports it. `None` when there is no git repository here,
/// which is the normal case inside a jj workspace.
async fn git_origin(pj_root: &Utf8Path) -> Option<String> {
    let out = Command::new("git")
        .args(["-C", pj_root.as_str(), "remote", "get-url", "origin"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `origin` as jj reports it.
///
/// A renri worktree is a jj *workspace*: it has a `.jj` and no `.git`, so
/// `git -C <root> remote get-url origin` fails there and the project looks
/// remote-less. It is not — it is the same GitHub repository the main
/// checkout points at, and answering "no github remote" for it is exactly
/// the wrong answer this skip exists to avoid giving.
///
/// `jj git remote list` prints `<name> <url>` per line.
async fn jj_origin(pj_root: &Utf8Path) -> Option<String> {
    let out = Command::new("jj")
        .args(["-R", pj_root.as_str(), "git", "remote", "list"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_jj_remote_list(&String::from_utf8_lossy(&out.stdout))
}

/// The `origin` url out of `jj git remote list` output.
fn parse_jj_remote_list(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next()? != "origin" {
            return None;
        }
        parts.next().map(str::to_string)
    })
}

/// The GitHub operations `[repo]` needs, behind a trait so the planning
/// logic can be exercised without a network or a `gh` binary.
#[async_trait]
pub trait GhApi: Send + Sync {
    async fn get_repo(&self, slug: &Slug) -> Result<Json>;
    async fn patch_repo(&self, slug: &Slug, body: &Json) -> Result<()>;
    async fn secret_names(&self, slug: &Slug) -> Result<Vec<String>>;
    async fn set_secret(&self, slug: &Slug, name: &str, value: &str) -> Result<()>;
}

/// The real implementation, shelling out to `gh` so kata inherits the
/// user's existing GitHub auth rather than growing a token of its own.
pub struct Gh;

#[async_trait]
impl GhApi for Gh {
    async fn get_repo(&self, slug: &Slug) -> Result<Json> {
        let out = gh(&["api", &format!("repos/{slug}")]).await?;
        serde_json::from_str(&out)
            .map_err(|e| Error::Repo(format!("could not parse `gh api repos/{slug}`: {e}")))
    }

    async fn patch_repo(&self, slug: &Slug, body: &Json) -> Result<()> {
        // The body goes over stdin rather than as repeated `-f key=value`
        // args: `-f` sends everything as a string, which turns `true`
        // into `"true"` and makes every boolean setting silently no-op.
        let json = serde_json::to_string(body)
            .map_err(|e| Error::Repo(format!("could not encode PATCH body: {e}")))?;
        gh_stdin(
            &[
                "api",
                "-X",
                "PATCH",
                &format!("repos/{slug}"),
                "--input",
                "-",
            ],
            &json,
        )
        .await
        .map(|_| ())
    }

    async fn secret_names(&self, slug: &Slug) -> Result<Vec<String>> {
        let out = gh(&[
            "api",
            &format!("repos/{slug}/actions/secrets"),
            "--jq",
            ".secrets[].name",
        ])
        .await?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    async fn set_secret(&self, slug: &Slug, name: &str, value: &str) -> Result<()> {
        // The value goes over stdin, never argv: process listings are
        // readable by other users on the machine.
        gh_stdin(&["secret", "set", name, "--repo", &slug.to_string()], value)
            .await
            .map(|_| ())
    }
}

async fn gh(args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .await
        .map_err(|e| Error::Repo(format!("could not run `gh`: {e}")))?;
    if !out.status.success() {
        return Err(Error::Repo(format!(
            "`gh {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// `gh` with a body on stdin. The body may be a secret, so it is never
/// echoed — not into an error message either.
async fn gh_stdin(args: &[&str], stdin: &str) -> Result<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Repo(format!("could not run `gh`: {e}")))?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| Error::Repo("could not open stdin for `gh`".to_string()))?
        .write_all(stdin.as_bytes())
        .await
        .map_err(|e| Error::Repo(format!("could not write to `gh` stdin: {e}")))?;
    // Dropping the handle closes the pipe; `gh` waits for EOF otherwise.
    drop(child.stdin.take());

    let out = child
        .wait_with_output()
        .await
        .map_err(|e| Error::Repo(format!("`gh` did not finish: {e}")))?;
    if !out.status.success() {
        return Err(Error::Repo(format!(
            "`gh {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Read the live repository and report what would change.
/// Reject `[repo.settings]` values the pass-through cannot honestly carry.
///
/// Every documented field of `PATCH repos/{owner}/{repo}` is a scalar, and
/// nothing stops a manifest author writing an array anyway. Without this
/// they would get a confusing `gh api` failure — or worse, a stringified
/// array quietly PATCHed — instead of being told upfront.
pub fn validate_settings(settings: &toml::Table) -> Result<()> {
    for (key, value) in settings {
        if value.is_array() || value.is_table() {
            return Err(Error::Repo(format!(
                "`{key}` is a {}; [repo.settings] hands values straight to the GitHub API, which takes only scalars here",
                value.type_str()
            )));
        }
    }
    Ok(())
}

pub async fn plan(
    api: &dyn GhApi,
    slug: &Slug,
    settings: &toml::Table,
    secret_names: &[String],
) -> Result<RepoPlan> {
    validate_settings(settings)?;
    let mut out = RepoPlan::default();
    if !settings.is_empty() {
        let actual = api.get_repo(slug).await?;
        out.settings = plan_settings(settings, &actual);
    }
    if !secret_names.is_empty() {
        let existing = api.secret_names(slug).await?;
        out.secrets = plan_secret_names(secret_names, &existing);
    }
    Ok(out)
}

/// Converge the repository onto the plan.
///
/// `secret_values` is looked up by name and is the only place a value
/// exists; it is never returned, logged, or put in a diff.
pub async fn execute(
    api: &dyn GhApi,
    slug: &Slug,
    desired: &toml::Table,
    plan: &RepoPlan,
    secret_values: &[(String, String)],
) -> Result<()> {
    // Only the keys that actually differ are sent, so an apply that
    // changes one field does not rewrite the rest of the repository.
    let mut body = serde_json::Map::new();
    for change in &plan.settings {
        if let SettingChange::Update { key, .. } = change {
            if let Some(want) = desired.get(key) {
                body.insert(key.clone(), toml_to_json(want));
            }
        }
    }
    if !body.is_empty() {
        api.patch_repo(slug, &Json::Object(body)).await?;
    }

    for secret in &plan.secrets {
        let value = secret_values
            .iter()
            .find(|(name, _)| name == &secret.name)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| {
                Error::Repo(format!("no value resolved for secret `{}`", secret.name))
            })?;
        api.set_secret(slug, &secret.name, value).await?;
    }
    Ok(())
}

fn json_matches_toml(actual: &Json, desired: &toml::Value) -> bool {
    match (actual, desired) {
        (Json::Bool(a), toml::Value::Boolean(d)) => a == d,
        (Json::String(a), toml::Value::String(d)) => a == d,
        (Json::Number(a), toml::Value::Integer(d)) => a.as_i64() == Some(*d),
        (Json::Number(a), toml::Value::Float(d)) => a.as_f64() == Some(*d),
        _ => false,
    }
}

fn toml_to_json(v: &toml::Value) -> Json {
    match v {
        toml::Value::Boolean(b) => Json::Bool(*b),
        toml::Value::Integer(i) => Json::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        toml::Value::String(s) => Json::String(s.clone()),
        other => Json::String(other.to_string()),
    }
}

fn show_json(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn show_toml(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(src: &str) -> toml::Table {
        src.parse::<toml::Table>().unwrap()
    }

    #[test]
    fn parse_slug_handles_the_url_forms_git_hands_out() {
        let want = Slug {
            owner: "yukimemi".into(),
            repo: "tsumiki".into(),
        };
        for url in [
            "https://github.com/yukimemi/tsumiki.git",
            "https://github.com/yukimemi/tsumiki",
            "git@github.com:yukimemi/tsumiki.git",
            "git@github.com:yukimemi/tsumiki",
            "ssh://git@github.com/yukimemi/tsumiki.git",
            "https://github.com/yukimemi/tsumiki/",
        ] {
            assert_eq!(parse_slug(url).as_ref(), Some(&want), "url: {url}");
        }
    }

    #[test]
    fn jj_remote_list_yields_the_origin_url() {
        let out = "origin https://github.com/yukimemi/tsumiki.git\n";
        assert_eq!(
            parse_jj_remote_list(out).as_deref(),
            Some("https://github.com/yukimemi/tsumiki.git")
        );
    }

    #[test]
    fn jj_remote_list_picks_origin_out_of_several_remotes() {
        let out =
            "upstream https://github.com/other/x.git\norigin git@github.com:yukimemi/tsumiki.git\n";
        assert_eq!(
            parse_jj_remote_list(out).as_deref(),
            Some("git@github.com:yukimemi/tsumiki.git")
        );
    }

    #[test]
    fn jj_remote_list_without_an_origin_is_none() {
        assert_eq!(
            parse_jj_remote_list("upstream https://example.com/x.git\n"),
            None
        );
        assert_eq!(parse_jj_remote_list(""), None);
        // A name that merely starts with "origin" is a different remote.
        assert_eq!(
            parse_jj_remote_list("origin2 https://example.com/x.git\n"),
            None
        );
    }

    #[test]
    fn parse_slug_skips_other_forges_rather_than_guessing() {
        assert_eq!(parse_slug("git@gitlab.com:yukimemi/tsumiki.git"), None);
        assert_eq!(parse_slug("https://codeberg.org/yukimemi/tsumiki"), None);
        assert_eq!(parse_slug("/srv/git/bare.git"), None);
        assert_eq!(parse_slug("https://github.com/yukimemi"), None);
    }

    #[test]
    fn settings_that_already_match_produce_no_work() {
        let desired = table("delete_branch_on_merge = true\nallow_merge_commit = false");
        let actual = serde_json::json!({
            "delete_branch_on_merge": true,
            "allow_merge_commit": false,
        });
        assert!(plan_settings(&desired, &actual).is_empty());
    }

    #[test]
    fn a_differing_setting_reports_both_sides() {
        let desired = table("delete_branch_on_merge = true");
        let actual = serde_json::json!({ "delete_branch_on_merge": false });
        assert_eq!(
            plan_settings(&desired, &actual),
            vec![SettingChange::Update {
                key: "delete_branch_on_merge".into(),
                from: "false".into(),
                to: "true".into(),
            }]
        );
    }

    #[test]
    fn a_key_the_api_never_returns_is_flagged_as_a_probable_typo() {
        let desired = table("delete_branch_on_merged = true");
        let actual = serde_json::json!({ "delete_branch_on_merge": false });
        assert_eq!(
            plan_settings(&desired, &actual),
            vec![SettingChange::Unknown {
                key: "delete_branch_on_merged".into()
            }]
        );
    }

    #[test]
    fn strings_and_numbers_compare_across_the_toml_json_boundary() {
        let desired = table("squash_merge_commit_title = \"PR_TITLE\"");
        let same = serde_json::json!({ "squash_merge_commit_title": "PR_TITLE" });
        assert!(plan_settings(&desired, &same).is_empty());

        let differs = serde_json::json!({ "squash_merge_commit_title": "COMMIT_OR_PR_TITLE" });
        assert_eq!(plan_settings(&desired, &differs).len(), 1);

        let n = table("some_count = 3");
        assert!(plan_settings(&n, &serde_json::json!({ "some_count": 3 })).is_empty());
        assert_eq!(
            plan_settings(&n, &serde_json::json!({ "some_count": 4 })).len(),
            1
        );
    }

    #[test]
    fn a_present_secret_is_not_work_and_a_missing_one_is() {
        let desired = vec![
            "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
            "NPM_TOKEN".to_string(),
        ];
        let existing = vec!["NPM_TOKEN".to_string()];
        assert_eq!(
            plan_secret_names(&desired, &existing),
            vec![SecretChange {
                name: "CLAUDE_CODE_OAUTH_TOKEN".into()
            }]
        );
    }

    #[test]
    fn rendered_lines_never_carry_a_secret_value() {
        let plan = RepoPlan {
            settings: vec![SettingChange::Update {
                key: "delete_branch_on_merge".into(),
                from: "false".into(),
                to: "true".into(),
            }],
            secrets: vec![SecretChange {
                name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            }],
        };
        assert_eq!(
            plan.work_lines(),
            vec![
                "delete_branch_on_merge: false → true",
                "CLAUDE_CODE_OAUTH_TOKEN: absent → present",
            ]
        );
    }

    #[test]
    fn a_typo_is_a_warning_and_not_work() {
        // `execute` PATCHes only `Update` entries, so a plan holding
        // nothing but an `Unknown` changes nothing on GitHub — and must
        // not be reported as though it had.
        let plan = RepoPlan {
            settings: vec![SettingChange::Unknown {
                key: "delete_branch_on_merged".into(),
            }],
            secrets: vec![],
        };
        assert!(!plan.has_work());
        assert!(plan.work_lines().is_empty());
        assert_eq!(
            plan.warning_lines(),
            vec!["warn: `delete_branch_on_merged` is not a field the API returns — typo?"]
        );
    }

    #[test]
    fn a_warning_alongside_real_work_does_not_hide_the_work() {
        let plan = RepoPlan {
            settings: vec![
                SettingChange::Unknown { key: "typo".into() },
                SettingChange::Update {
                    key: "delete_branch_on_merge".into(),
                    from: "false".into(),
                    to: "true".into(),
                },
            ],
            secrets: vec![],
        };
        assert!(plan.has_work());
        assert_eq!(plan.work_lines().len(), 1);
        assert_eq!(plan.warning_lines().len(), 1);
    }

    #[test]
    fn a_missing_secret_alone_still_counts_as_work() {
        let plan = RepoPlan {
            settings: vec![],
            secrets: vec![SecretChange {
                name: "TOKEN".into(),
            }],
        };
        assert!(plan.has_work());
    }

    #[test]
    fn every_not_ready_state_can_explain_itself() {
        // These strings end up in a `[repo] (...)` row, so they have to
        // read as an explanation rather than an error code.
        assert_eq!(Readiness::NoGh.reason(), "gh is not installed");
        assert_eq!(
            Readiness::NotAuthenticated.reason(),
            "gh is not authenticated"
        );
        assert_ne!(Readiness::Ready, Readiness::NoGh);
    }

    #[test]
    fn a_non_scalar_setting_is_refused_upfront() {
        let bad = table(r#"topics = ["a", "b"]"#);
        let err = validate_settings(&bad).unwrap_err().to_string();
        assert!(err.contains("topics"), "{err}");
        assert!(validate_settings(&table("delete_branch_on_merge = true")).is_ok());
    }

    #[test]
    fn booleans_survive_the_trip_into_a_patch_body() {
        // `-f key=value` would stringify these; the PATCH body has to
        // keep real JSON booleans or GitHub silently ignores them.
        assert_eq!(toml_to_json(&toml::Value::Boolean(true)), Json::Bool(true));
        assert_eq!(
            toml_to_json(&toml::Value::String("x".into())),
            Json::String("x".into())
        );
    }
}
