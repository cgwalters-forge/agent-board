//! Forge-neutral records and deterministic, preview-only reconciliation.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use url::Url;

pub const SNAPSHOT_SCHEMA: &str = "board-snapshot/v1";
pub const PLAN_SCHEMA: &str = "board-plan/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
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
            parts.len() >= 2
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

    pub fn display(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChangeRef {
    pub item: ItemRef,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunRef {
    pub host: String,
    pub repository: String,
    pub id: u64,
    pub attempt: u32,
    pub workflow_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub url: Url,
    pub observed_at: String,
    pub provenance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub complete: bool,
    pub observed_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub identity: ItemRef,
    pub title: String,
    pub state: String,
    #[serde(deserialize_with = "required_nullable")]
    pub status: Option<Status>,
    #[serde(deserialize_with = "required_nullable")]
    pub priority: Option<Priority>,
    #[serde(deserialize_with = "required_nullable")]
    pub turn: Option<Turn>,
    pub fields_complete: bool,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

// A missing field is unknown input, not an observed null. Using a custom
// deserializer prevents serde's implicit default for Option fields.
fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub project: Url,
    pub repositories: Vec<String>,
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Precondition {
    pub item: ItemRef,
    pub field: BoardField,
    pub expected: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Intent {
    SetStatus { item: ItemRef, value: Status },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub key: String,
    pub reason: String,
    pub intent: Intent,
    pub prerequisites: Vec<Precondition>,
    pub evidence: Vec<Evidence>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
    fn neutral_identity_and_project() {
        let identity = ItemRef {
            host: "forge.example".into(),
            repository: "group/subgroup/repository".into(),
            number: 1,
        };
        identity.validate().unwrap();
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.project = Url::parse("https://forge.example/group/board").unwrap();
        snapshot.items[0].identity = identity;
        snapshot.validate().unwrap();
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
        snapshot.project = Url::parse("http://forge.example/board").unwrap();
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn unknown_snapshot_version_is_rejected() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.schema = "board-snapshot/v2".into();
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn unknown_snapshot_fields_are_rejected() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        for pointer in ["", "/coverage", "/items/0", "/items/0/identity"] {
            let mut value = original.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), true.into());
            assert!(
                serde_json::from_value::<Snapshot>(value).is_err(),
                "{pointer}"
            );
        }
        let mut unknown_evidence = original;
        unknown_evidence["items"][0]["evidence"] = serde_json::json!([{
            "url": "https://forge.example/evidence",
            "observed_at": "2026-10-08T00:00:00Z",
            "provenance": "fixture",
            "unknown": true,
        }]);
        assert!(serde_json::from_value::<Snapshot>(unknown_evidence).is_err());
    }

    #[test]
    fn missing_status_is_not_observed_null() {
        let original: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let mut missing = original.clone();
        missing["items"][0]
            .as_object_mut()
            .unwrap()
            .remove("status");
        assert!(serde_json::from_value::<Snapshot>(missing.clone()).is_err());
        missing["items"][0]["sta tus"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Snapshot>(missing).is_err());
        let mut explicit_null = original;
        explicit_null["items"][0]["status"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<Snapshot>(explicit_null)
                .unwrap()
                .items[0]
                .status
                .is_none()
        );
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
