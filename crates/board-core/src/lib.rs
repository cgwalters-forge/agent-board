//! Forge-neutral records and deterministic, preview-only reconciliation.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use url::Url;

pub const SNAPSHOT_SCHEMA: &str = "board-snapshot/v1";
pub const PLAN_SCHEMA: &str = "board-plan/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemRef {
    pub host: String,
    pub repository: String,
    pub number: u64,
}

impl ItemRef {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.is_empty()
                && url::Host::parse(&self.host).is_ok()
                && !self.host.contains(['/', ':', '@']),
            "invalid issue host"
        );
        let parts: Vec<_> = self.repository.split('/').collect();
        ensure!(
            parts.len() == 2
                && parts.iter().all(|part| !part.is_empty()
                    && *part != "."
                    && *part != ".."
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))),
            "invalid issue repository"
        );
        ensure!(self.number > 0, "invalid issue number");
        Ok(())
    }

    pub fn parse(input: &str, repository: Option<&str>) -> Result<Self> {
        let (host, repository, number) = if input.contains("://") {
            let url = Url::parse(input)?;
            ensure!(url.scheme() == "https", "issue URLs must use HTTPS");
            ensure!(
                url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "issue URL must be canonical"
            );
            let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
            ensure!(
                parts.len() == 4 && parts[2] == "issues",
                "expected an issue URL"
            );
            (
                url.host_str().context("missing issue URL host")?.to_owned(),
                format!("{}/{}", parts[0], parts[1]),
                parts[3].parse()?,
            )
        } else {
            let (repo, number) = match input.rsplit_once('#') {
                Some(parts) => parts,
                None => (
                    repository.ok_or_else(|| anyhow::anyhow!("bare numbers require --repo"))?,
                    input,
                ),
            };
            ("github.com".into(), repo.into(), number.parse()?)
        };
        ensure!(
            number > 0
                && repository.split('/').count() == 2
                && !repository.split('/').any(str::is_empty),
            "invalid issue identity"
        );
        let identity = Self {
            host,
            repository,
            number,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn display(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ChangeRef {
    pub item: ItemRef,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RunRef {
    pub host: String,
    pub repository: String,
    pub id: u64,
    pub attempt: u32,
    pub workflow_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Actor {
    pub host: String,
    pub login: String,
    pub provenance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoardField {
    Status,
    Priority,
    Turn,
    Ask,
    Result,
    BlockedBy,
    Next,
    News,
    Run,
    Slot,
    BudgetTokens,
    SpentTokens,
    Request,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Status {
    Triage,
    Backlog,
    Todo,
    #[serde(rename = "In Progress")]
    InProgress,
    Draft,
    #[serde(rename = "In Review")]
    InReview,
    Blocked,
    Done,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Turn {
    Coordinator,
    Worker,
    Operator,
    External,
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Priority {
    P0,
    P1,
    P2,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Evidence {
    pub url: Url,
    pub observed_at: String,
    pub provenance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Coverage {
    pub complete: bool,
    pub observed_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Item {
    pub identity: ItemRef,
    pub title: String,
    pub state: String,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub turn: Option<Turn>,
    pub fields_complete: bool,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Snapshot {
    pub schema: String,
    pub project: Url,
    pub clock: String,
    pub coverage: Coverage,
    pub items: Vec<Item>,
}

impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == SNAPSHOT_SCHEMA,
            "unsupported snapshot schema: {}",
            self.schema
        );
        ensure!(
            self.project.scheme() == "https",
            "project must be an HTTPS URL"
        );
        ensure!(
            self.project.username().is_empty()
                && self.project.password().is_none()
                && self.project.query().is_none()
                && self.project.fragment().is_none(),
            "project URL must be canonical"
        );
        if self.project.host_str() == Some("github.com") {
            let parts: Vec<_> = self.project.path().trim_matches('/').split('/').collect();
            ensure!(
                parts.len() == 4
                    && ["orgs", "users"].contains(&parts[0])
                    && !parts[1].is_empty()
                    && parts[2] == "projects"
                    && parts[3].parse::<u64>().is_ok_and(|n| n > 0),
                "invalid GitHub project URL"
            );
        }
        time::OffsetDateTime::parse(&self.clock, &time::format_description::well_known::Rfc3339)
            .context("invalid snapshot clock")?;
        time::OffsetDateTime::parse(
            &self.coverage.observed_at,
            &time::format_description::well_known::Rfc3339,
        )
        .context("invalid coverage timestamp")?;
        let mut seen = std::collections::BTreeSet::new();
        for item in &self.items {
            ensure!(seen.insert(&item.identity), "duplicate item identity");
            item.identity.validate()?;
            ensure!(
                ["open", "closed"].contains(&item.state.as_str()),
                "invalid issue state"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Capability {
    ProjectFieldEdit,
    CreateIssue,
    CloseIssue,
    TypedRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Policy {
    pub project: Url,
    pub repositories: Vec<String>,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Precondition {
    pub item: ItemRef,
    pub field: BoardField,
    pub expected: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    SetStatus { item: ItemRef, value: Status },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Action {
    pub key: String,
    pub reason: String,
    pub intent: Intent,
    pub prerequisites: Vec<Precondition>,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Plan {
    pub schema: String,
    pub project: Url,
    pub actions: Vec<Action>,
    pub diagnostics: BTreeMap<String, String>,
}

pub fn reconcile(snapshot: &Snapshot, policy: &Policy, now: &str) -> Result<Plan> {
    snapshot.validate()?;
    ensure!(
        snapshot.project == policy.project,
        "policy project mismatch"
    );
    ensure!(
        now == snapshot.clock,
        "clock must match injected snapshot clock"
    );
    let mut plan = Plan {
        schema: PLAN_SCHEMA.into(),
        project: snapshot.project.clone(),
        actions: vec![],
        diagnostics: BTreeMap::new(),
    };
    if !policy.capabilities.contains(&Capability::ProjectFieldEdit) {
        bail!("unsupported_capability: project field edits");
    }
    for item in &snapshot.items {
        if !item.fields_complete {
            plan.diagnostics.insert(
                item.identity.display(),
                "missing_data: incomplete fields".into(),
            );
            continue;
        }
        if item.status.is_some() || !policy.repositories.contains(&item.identity.repository) {
            continue;
        }
        ensure!(
            Some(item.identity.host.as_str()) == snapshot.project.host_str(),
            "item host differs from project host"
        );
        let key_input =
            serde_json::to_vec(&(snapshot.project.as_str(), &item.identity, "missing_status"))?;
        plan.actions.push(Action {
            key: format!("{:x}", Sha256::digest(key_input)),
            reason: "missing_status".into(),
            intent: Intent::SetStatus {
                item: item.identity.clone(),
                value: Status::Triage,
            },
            prerequisites: vec![Precondition {
                item: item.identity.clone(),
                field: BoardField::Status,
                expected: None,
            }],
            evidence: item.evidence.clone(),
        });
    }
    plan.actions.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_table() {
        for (status, complete, allowed, count) in [
            (None, true, true, 1),
            (Some(Status::Triage), true, true, 0),
            (Some(Status::Done), true, true, 0),
            (None, false, true, 0),
            (None, true, false, 0),
        ] {
            let mut snapshot: Snapshot =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            snapshot.items.truncate(1);
            snapshot.items[0].status = status;
            snapshot.items[0].fields_complete = complete;
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: if allowed {
                    vec!["example/intake".into()]
                } else {
                    vec![]
                },
                capabilities: vec![Capability::ProjectFieldEdit],
            };
            let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert_eq!(plan.actions.len(), count);
            assert_eq!(
                serde_json::to_vec(&plan).unwrap(),
                serde_json::to_vec(&reconcile(&snapshot, &policy, &snapshot.clock).unwrap())
                    .unwrap()
            );
            if count == 1 {
                snapshot.items[0].status = Some(Status::Triage);
                assert!(
                    reconcile(&snapshot, &policy, &snapshot.clock)
                        .unwrap()
                        .actions
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn identity_table() {
        for input in [
            "example/intake#1",
            "https://github.com/example/intake/issues/1",
        ] {
            assert_eq!(ItemRef::parse(input, None).unwrap().number, 1);
        }
        for input in [
            "1",
            "example/intake#0",
            "https://github.com/example/intake/pull/1",
        ] {
            assert!(ItemRef::parse(input, None).is_err());
        }
    }

    #[test]
    fn malformed_snapshot_table() {
        for (field, value) in [
            ("repository", "not-a-repository"),
            ("repository", "example/.."),
            ("host", "github.com/path"),
        ] {
            let mut value_json: serde_json::Value =
                serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
            value_json["items"][0]["identity"][field] = value.into();
            let snapshot: Snapshot = serde_json::from_value(value_json).unwrap();
            assert!(snapshot.validate().is_err());
        }
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.project = Url::parse("https://github.com/not-a-project").unwrap();
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn ordering_is_input_independent() {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec!["example/intake".into()],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let mut first = snapshot.clone();
        for item in &mut first.items {
            item.status = None;
        }
        let mut second = first.clone();
        second.items.reverse();
        assert_eq!(
            reconcile(&first, &policy, &first.clock).unwrap(),
            reconcile(&second, &policy, &second.clock).unwrap()
        );
    }
}
