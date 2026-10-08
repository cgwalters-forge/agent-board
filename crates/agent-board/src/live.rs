//! Explicitly privileged staging fixture; not available through a board adapter.
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

const OWNER: &str = "cgwalters-forge-stage";
const MARKER: &str = "agent-board-fixture/v1\n";
const READ: &str = "query($id:ID!){node(id:$id){... on ProjectV2{id title url readme createdAt owner{... on Organization{login}}}}}";
const DELETE: &str =
    "mutation($id:ID!){deleteProjectV2(input:{projectId:$id}){deletedProjectV2Id}}";

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    organization: String,
    #[arg(long, value_name = "OWNER/REPO")]
    scratch_repository: String,
    #[arg(long, default_value = "agent-board-ci-")]
    name_prefix: String,
    /// Reserved for sweeping with independently trusted creation receipts (not implemented).
    #[arg(long)]
    sweep: bool,
}

#[derive(Debug)]
pub struct AuthenticationRequired;

impl std::fmt::Display for AuthenticationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication required: provide a valid GH_TOKEN or GITHUB_TOKEN in the protected live environment"
        )
    }
}

impl std::error::Error for AuthenticationRequired {}

pub fn error_exit_code(error: &anyhow::Error) -> i32 {
    if error.downcast_ref::<AuthenticationRequired>().is_some() {
        4
    } else {
        1
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    owner: String,
    repository: String,
    prefix: String,
    nonce: String,
    run_id: u64,
    attempt: u32,
    created: i64,
}

trait Transport {
    fn graphql(&mut self, query: &str, variables: Value) -> Result<Value>;
}

fn classify_transport_error(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(401) => AuthenticationRequired.into(),
        ureq::Error::StatusCode(403) => anyhow::anyhow!(
            "GitHub denied the request (HTTP 403): check Organization Projects read/write and repository Issues read/write permissions on the staging token, organization approval, and rate limits"
        ),
        // Do not include server bodies, request headers or arbitrary transport text.
        ureq::Error::StatusCode(status) => {
            anyhow::anyhow!("GitHub HTTP request failed (status {status})")
        }
        _ => anyhow::anyhow!("GitHub transport failed"),
    }
}

struct GitHub {
    token: String,
    agent: ureq::Agent,
}

impl Transport for GitHub {
    fn graphql(&mut self, query: &str, variables: Value) -> Result<Value> {
        let response: Value = self
            .agent
            .post("https://api.github.com/graphql")
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("User-Agent", "agent-board-fixture")
            .send_json(json!({"query":query,"variables":variables}))
            .map_err(classify_transport_error)
            .context("GitHub GraphQL request failed")?
            .body_mut()
            .read_json()
            .context("decode GraphQL response")?;
        // Do not echo server errors, which can contain arbitrary input.
        ensure!(
            response.get("errors").is_none(),
            "GitHub returned GraphQL errors; fixture state may be ambiguous"
        );
        response
            .get("data")
            .cloned()
            .context("missing GraphQL data")
    }
}

fn validate_options(options: &Options) -> Result<()> {
    ensure!(
        options.organization == OWNER,
        "live fixtures are restricted to {OWNER}"
    );
    let parts: Vec<_> = options.scratch_repository.split('/').collect();
    ensure!(
        parts.len() == 2
            && parts[0] == OWNER
            && !parts[1].is_empty()
            && parts[1]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)),
        "scratch repository must belong to staging organization"
    );
    ensure!(
        options.name_prefix.starts_with("agent-board-ci-")
            && options.name_prefix.len() <= 64
            && options
                .name_prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "name prefix must start with agent-board-ci- and contain only ASCII letters, numbers and hyphens"
    );
    Ok(())
}

pub fn run(options: &Options) -> Result<()> {
    validate_options(options)?;
    if options.sweep {
        return sweep();
    }
    let token = select_token(
        std::env::var("GH_TOKEN").ok(),
        std::env::var("GITHUB_TOKEN").ok(),
    )?;
    let mut transport = GitHub {
        token,
        agent: ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into(),
    };
    let now = OffsetDateTime::now_utc().unix_timestamp();
    ensure!(
        std::env::var("GITHUB_REPOSITORY").ok().as_deref()
            == Some(options.scratch_repository.as_str()),
        "GITHUB_REPOSITORY must match the scratch repository for run provenance"
    );
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate fixture nonce"))?;
    let nonce = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let receipt = Receipt {
        owner: options.organization.clone(),
        repository: options.scratch_repository.clone(),
        prefix: options.name_prefix.clone(),
        nonce,
        run_id: std::env::var("GITHUB_RUN_ID")
            .context("GITHUB_RUN_ID required for cleanup provenance")?
            .parse()?,
        attempt: std::env::var("GITHUB_RUN_ATTEMPT")
            .context("GITHUB_RUN_ATTEMPT required")?
            .parse()?,
        created: now,
    };
    lifecycle(&mut transport, options, &receipt)
}

fn select_token(gh_token: Option<String>, github_token: Option<String>) -> Result<String> {
    gh_token
        .filter(|token| !token.is_empty())
        .or_else(|| github_token.filter(|token| !token.is_empty()))
        .ok_or_else(|| AuthenticationRequired.into())
}

fn title(receipt: &Receipt) -> String {
    format!(
        "{}{}-{}-{}",
        receipt.prefix, receipt.run_id, receipt.attempt, receipt.nonce
    )
}

fn description(receipt: &Receipt) -> Result<String> {
    Ok(format!("{MARKER}{}", serde_json::to_string(receipt)?))
}

fn verify(project: &Value, id: &str, receipt: &Receipt, options: &Options) -> Result<()> {
    validate_options(options)?;
    ensure!(
        receipt.owner == options.organization
            && receipt.repository == options.scratch_repository
            && receipt.prefix == options.name_prefix,
        "fixture provenance scope mismatch"
    );
    ensure!(
        receipt.nonce.len() == 32
            && receipt.nonce.chars().all(|c| c.is_ascii_hexdigit())
            && receipt.run_id > 0
            && receipt.attempt > 0,
        "invalid fixture nonce/run provenance"
    );
    ensure!(
        project["id"].as_str() == Some(id) && !id.is_empty(),
        "project identity mismatch"
    );
    ensure!(
        project["owner"]["login"].as_str() == Some(OWNER),
        "project owner mismatch"
    );
    ensure!(
        project["title"].as_str() == Some(title(receipt).as_str()),
        "project name/nonce mismatch"
    );
    ensure!(
        project["readme"].as_str() == Some(description(receipt)?.as_str()),
        "project provenance mismatch"
    );
    let url = project["url"].as_str().context("missing project URL")?;
    let number = url
        .strip_prefix(&format!("https://github.com/orgs/{OWNER}/projects/"))
        .context("project URL owner mismatch")?;
    ensure!(
        number.parse::<u64>().is_ok_and(|n| n > 0),
        "invalid project URL"
    );
    let created = OffsetDateTime::parse(
        project["createdAt"]
            .as_str()
            .context("missing creation time")?,
        &time::format_description::well_known::Rfc3339,
    )?
    .unix_timestamp();
    ensure!(
        created.abs_diff(receipt.created) <= 300,
        "fixture creation timestamp mismatch"
    );
    Ok(())
}

fn cleanup(t: &mut impl Transport, id: &str, receipt: &Receipt, options: &Options) -> Result<()> {
    let project = t.graphql(READ, json!({"id":id}))?;
    verify(&project["node"], id, receipt, options)?;
    let deleted = t.graphql(DELETE, json!({"id":id}))?;
    ensure!(
        deleted["deleteProjectV2"]["deletedProjectV2Id"].as_str() == Some(id),
        "delete receipt mismatch"
    );
    let after = t.graphql(READ, json!({"id":id}))?;
    ensure!(
        after.get("node").is_some_and(Value::is_null),
        "project deletion not confirmed"
    );
    Ok(())
}

fn lifecycle(t: &mut impl Transport, options: &Options, receipt: &Receipt) -> Result<()> {
    let owner = t.graphql("query($owner:String!,$repo:String!){organization(login:$owner){id} repository(owner:$owner,name:$repo){nameWithOwner}}", json!({"owner":options.organization,"repo":options.scratch_repository.split('/').nth(1)}))?;
    ensure!(
        owner["repository"]["nameWithOwner"].as_str() == Some(options.scratch_repository.as_str()),
        "scratch repository not readable"
    );
    let owner_id = owner["organization"]["id"]
        .as_str()
        .context("missing staging organization ID")?;
    let created = t.graphql("mutation($owner:ID!,$title:String!){createProjectV2(input:{ownerId:$owner,title:$title}){projectV2{id}}}", json!({"owner":owner_id,"title":title(receipt)}))?;
    let id = created["createProjectV2"]["projectV2"]["id"]
        .as_str()
        .context("missing created project ID; creation may be ambiguous")?;
    // Record the ID immediately. A crash before provenance is installed needs manual recovery,
    // not prefix-only deletion by the sweeper.
    println!("Created staging fixture {id}; installing cleanup provenance.");
    let result = (|| -> Result<()> {
        t.graphql("mutation($id:ID!,$description:String!){updateProjectV2(input:{projectId:$id,readme:$description}){projectV2{id}}}", json!({"id":id,"description":description(receipt)?}))?;
        let read = t.graphql(READ, json!({"id":id}))?;
        verify(&read["node"], id, receipt, options)
    })();
    let cleanup_result = cleanup(t, id, receipt, options);
    match (result, cleanup_result) {
        (Ok(()), Ok(())) => {
            println!("Fixture create/read/delete verified.");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error.context("fixture failed; cleanup succeeded")),
        (result, Err(error)) => bail!(
            "cleanup failed for recorded project {id}: {error:#}; fixture result: {}",
            if result.is_ok() { "passed" } else { "failed" }
        ),
    }
}

fn sweep() -> Result<()> {
    // The editable project README and a completed run do not authenticate creation.
    // No trusted receipt backend exists yet. Refuse before even constructing a
    // credential-bearing client, rather than expose a self-attested deletion path.
    bail!(
        "unsupported_capability: sweeping requires independently trusted creation receipts binding project ID, nonce, run/attempt and approved workflow identity; use manual recovery"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Recorded {
        responses: VecDeque<Value>,
        deletes: usize,
    }

    impl Transport for Recorded {
        fn graphql(&mut self, query: &str, _: Value) -> Result<Value> {
            if query == DELETE {
                self.deletes += 1;
            }
            self.responses.pop_front().context("unexpected request")
        }
    }

    fn fixture() -> (Options, Receipt, Value) {
        let options = Options {
            organization: OWNER.into(),
            scratch_repository: format!("{OWNER}/board-test"),
            name_prefix: "agent-board-ci-".into(),
            sweep: false,
        };
        let receipt = Receipt {
            owner: OWNER.into(),
            repository: options.scratch_repository.clone(),
            prefix: options.name_prefix.clone(),
            nonce: "0123456789abcdef0123456789abcdef".into(),
            run_id: 123,
            attempt: 1,
            created: 0,
        };
        let project = json!({"id":"P_fixture","title":title(&receipt),"url":format!("https://github.com/orgs/{OWNER}/projects/1"),"readme":description(&receipt).unwrap(),"owner":{"login":OWNER},"createdAt":"1970-01-01T00:00:00Z"});
        (options, receipt, project)
    }

    #[test]
    fn cleanup_refusal_table() {
        let (options, receipt, project) = fixture();
        for (field, value) in [
            ("id", json!("wrong")),
            ("owner", json!({"login":"cgwalters-forge"})),
            ("title", json!("agent-board-ci-forged")),
            ("readme", json!("copied marker")),
            (
                "url",
                json!("https://github.com/orgs/cgwalters-forge/projects/1"),
            ),
            ("createdAt", json!("2026-10-08T00:00:00Z")),
        ] {
            let mut changed = project.clone();
            changed[field] = value;
            let mut transport = Recorded {
                responses: vec![json!({"node":changed})].into(),
                deletes: 0,
            };
            assert!(cleanup(&mut transport, "P_fixture", &receipt, &options).is_err());
            assert_eq!(transport.deletes, 0);
        }
    }

    #[test]
    fn recorded_lifecycle() {
        let (options, receipt, project) = fixture();
        let mut transport = Recorded { responses: vec![
            json!({"organization":{"id":"O_stage"},"repository":{"nameWithOwner":options.scratch_repository}}),
            json!({"createProjectV2":{"projectV2":{"id":"P_fixture"}}}),
            json!({"updateProjectV2":{"projectV2":{"id":"P_fixture"}}}),
            json!({"node":project}), json!({"node":project}),
            json!({"deleteProjectV2":{"deletedProjectV2Id":"P_fixture"}}), json!({"node":null}),
        ].into(), deletes: 0 };
        lifecycle(&mut transport, &options, &receipt).unwrap();
        assert_eq!(transport.deletes, 1);
        assert!(transport.responses.is_empty());
    }

    #[test]
    fn fully_forged_receipt_cannot_authorize_sweeping() {
        let (mut options, receipt, mut project) = fixture();
        project["id"] = "P_nonfixture".into();
        // This forged object passes all of the old self-attestation checks,
        // including a valid nonce, timestamp and real-looking run identity.
        verify(&project, "P_nonfixture", &receipt, &options).unwrap();
        let decoded: Receipt = serde_json::from_str(
            project["readme"]
                .as_str()
                .unwrap()
                .strip_prefix(MARKER)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(decoded.run_id, 123);
        options.sweep = true;
        // No token, network, run-status lookup or delete is reachable in this mode.
        assert!(
            run(&options)
                .unwrap_err()
                .to_string()
                .contains("independently trusted creation receipts")
        );
    }

    #[test]
    fn extreme_timestamp_and_scope_refused() {
        let (mut options, mut receipt, mut project) = fixture();
        receipt.created = i64::MIN;
        project["readme"] = description(&receipt).unwrap().into();
        assert!(verify(&project, "P_fixture", &receipt, &options).is_err());
        options.organization = "cgwalters-forge".into();
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn http_authentication_table() {
        for (status, authentication) in [(401, true), (403, false), (429, false), (500, false)] {
            let error =
                classify_transport_error(ureq::Error::StatusCode(status)).context("request failed");
            assert_eq!(
                error.downcast_ref::<AuthenticationRequired>().is_some(),
                authentication
            );
            assert_eq!(error_exit_code(&error), if authentication { 4 } else { 1 });
            assert!(!format!("{error:#}").contains("response body"));
        }
    }

    #[test]
    fn token_precedence_table() {
        for (gh, github, expected) in [
            (Some("primary"), Some("fallback"), Some("primary")),
            (Some(""), Some("fallback"), Some("fallback")),
            (None, Some("fallback"), Some("fallback")),
            (Some("primary"), None, Some("primary")),
            (Some(""), Some(""), None),
            (None, None, None),
        ] {
            let result = select_token(gh.map(str::to_owned), github.map(str::to_owned));
            match expected {
                Some(value) => assert_eq!(result.unwrap(), value),
                None => assert_eq!(error_exit_code(&result.unwrap_err()), 4),
            }
        }
    }
}
