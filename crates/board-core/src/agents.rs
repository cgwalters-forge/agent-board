//! Assignment-aware proposal checks; callers supply trusted identity and inputs.
use crate::Snapshot;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_AGENT_POLICY: &str = include_str!("../../../policy/agents.toml");

fn is_assignment_field(name: &str) -> bool {
    // gh-aw title-cases words split on whitespace, '_' and '-', then resolves
    // fields case-insensitively. Only case variants of this single ASCII word
    // resolve to Agent: separators would introduce spaces, not remove them.
    name.eq_ignore_ascii_case("Agent")
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    pub enabled: bool,
    pub fields: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    pub agents: BTreeMap<String, AgentIdentity>,
}

impl AgentPolicy {
    pub fn parse(input: &str) -> Result<Self> {
        let policy: Self = toml::from_str(input).context("decode agent policy TOML")?;
        for (name, agent) in &policy.agents {
            ensure!(!name.is_empty(), "empty agent identity");
            let fields: BTreeSet<_> = agent.fields.iter().collect();
            ensure!(
                fields.len() == agent.fields.len(),
                "duplicate field for {name}"
            );
            ensure!(
                agent
                    .fields
                    .iter()
                    .all(|f| !f.is_empty() && !is_assignment_field(f)),
                "Agent is never editable (or empty field) for {name}"
            );
            let outputs: BTreeSet<_> = agent.outputs.iter().collect();
            ensure!(
                outputs.len() == agent.outputs.len(),
                "duplicate output for {name}"
            );
            ensure!(
                agent
                    .outputs
                    .iter()
                    .all(|o| matches!(o.as_str(), "update_project" | "close_issue")),
                "unsupported output type for {name}"
            );
        }
        Ok(policy)
    }

    pub fn identity(&self, name: &str) -> Result<&AgentIdentity> {
        let agent = self
            .agents
            .get(name)
            .with_context(|| format!("unknown agent identity: {name}"))?;
        ensure!(agent.enabled, "disabled agent identity: {name}");
        Ok(agent)
    }

    /// Normalize the reader's raw Agent field, refusing ambiguous or unknown values.
    pub fn normalize(&self, snapshot: &mut Snapshot) -> Result<()> {
        let schemas: Vec<_> = snapshot
            .fields
            .iter()
            .filter(|f| f["name"] == "Agent")
            .collect();
        ensure!(schemas.len() <= 1, "duplicate Agent field");
        if let Some(schema) = schemas.first() {
            ensure!(
                schema["dataType"] == "SINGLE_SELECT",
                "Agent field is not single-select"
            );
        }
        for item in &mut snapshot.items {
            let assignment = if let Some(value) = item.fields.get("Agent") {
                let schema = schemas.first().context("missing Agent field schema")?;
                ensure!(
                    value["__typename"] == "ProjectV2ItemFieldSingleSelectValue",
                    "invalid Agent value"
                );
                let name = value["name"].as_str().context("missing Agent name")?;
                let id = value["optionId"]
                    .as_str()
                    .context("missing Agent option ID")?;
                ensure!(
                    schema["options"].as_array().is_some_and(|options| options
                        .iter()
                        .filter(|o| o["id"] == id && o["name"] == name)
                        .count()
                        == 1),
                    "unknown Agent option"
                );
                ensure!(
                    self.agents.contains_key(name),
                    "unknown Agent value: {name}"
                );
                Some(name.to_owned())
            } else {
                None
            };
            ensure!(
                item.assignment.is_none() || item.assignment == assignment,
                "assignment disagrees with Agent field for {}",
                item.identity.display()
            );
            item.assignment = assignment;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Proposal {
    #[serde(rename = "update_project")]
    UpdateProject {
        project: String,
        content_type: String,
        content_number: u64,
        target_repo: String,
        fields: BTreeMap<String, Value>,
    },
    #[serde(rename = "close_issue")]
    CloseIssue {
        target_repo: String,
        issue_number: u64,
    },
}

/// Refuse the entire batch on any disallowed output, target or field.
/// This assignment/name gate does not enforce byte/count bounds, field schema,
/// value types or option existence; independent validation is required before writes.
pub fn check_proposals(
    policy: &AgentPolicy,
    identity: &str,
    snapshot: &Snapshot,
    outputs: &[Value],
) -> Result<()> {
    let agent = policy.identity(identity)?;
    snapshot.validate()?;
    ensure!(snapshot.coverage.complete, "incomplete snapshot coverage");
    let mut observed = snapshot.clone();
    policy.normalize(&mut observed)?;
    for (index, output) in outputs.iter().enumerate() {
        let check = || -> Result<()> {
            let kind = output["type"].as_str().context("missing output type")?;
            ensure!(
                agent.outputs.iter().any(|o| o == kind),
                "output type not permitted: {kind}"
            );
            let proposal: Proposal =
                serde_json::from_value(output.clone()).context("decode proposal")?;
            let (repo, number, content_kind) = match &proposal {
                Proposal::UpdateProject {
                    project,
                    content_type,
                    content_number,
                    target_repo,
                    fields,
                } => {
                    ensure!(project == observed.project.as_str(), "project mismatch");
                    ensure!(!fields.is_empty(), "empty field edit");
                    for field in fields.keys() {
                        ensure!(!is_assignment_field(field), "Agent is never editable");
                        ensure!(agent.fields.contains(field), "field not permitted: {field}");
                    }
                    (target_repo, *content_number, content_type.as_str())
                }
                Proposal::CloseIssue {
                    target_repo,
                    issue_number,
                } => (target_repo, *issue_number, "issue"),
            };
            ensure!(
                matches!(content_kind, "issue" | "pull_request"),
                "unsupported content type"
            );
            let item = observed
                .items
                .iter()
                .find(|item| {
                    item.identity.host == "github.com"
                        && item.identity.repository == *repo
                        && item.identity.number == number
                        && item.content_kind == content_kind
                })
                .context("target is not a known project item")?;
            ensure!(item.fields_complete, "incomplete item fields");
            ensure!(
                item.assignment.as_deref() == Some(identity),
                "item {} is not assigned to {identity}",
                item.identity.display()
            );
            Ok(())
        };
        check().with_context(|| format!("proposal line {} refused", index + 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(assignment: Option<&str>) -> Snapshot {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot.fields.push(json!({"name":"Agent", "dataType":"SINGLE_SELECT", "options":[
            {"id":"worker", "name":"rust-worker"}, {"id":"peer", "name":"board-coordinator"}, {"id":"unknown", "name":"unknown"}
        ]}));
        snapshot.items[0].content_kind = "issue".into();
        if let Some(name) = assignment {
            let id = match name {
                "rust-worker" => "worker",
                "board-coordinator" => "peer",
                _ => "unknown",
            };
            snapshot.items[0].fields.insert("Agent".into(), json!({"__typename":"ProjectV2ItemFieldSingleSelectValue", "name":name, "optionId":id}));
        }
        snapshot
    }

    fn edit(field: &str) -> Value {
        json!({"type":"update_project", "project":"https://github.com/orgs/example/projects/1", "target_repo":"example/intake", "content_type":"issue", "content_number":1, "fields":{field:"value"}})
    }

    #[test]
    fn assignment_field_alias_table() {
        let snapshot = snapshot(Some("rust-worker"));
        for field in ["Agent", "agent", "AGENT", "aGeNt"] {
            let input = DEFAULT_AGENT_POLICY.replace("\"Result\"", &format!("\"{field}\""));
            let error = AgentPolicy::parse(&input).unwrap_err();
            assert!(
                error.to_string().contains("Agent is never editable"),
                "{field}"
            );

            // Also defend callers that construct or mutate a policy without parse.
            let mut policy = AgentPolicy::parse(DEFAULT_AGENT_POLICY).unwrap();
            policy
                .agents
                .get_mut("rust-worker")
                .unwrap()
                .fields
                .push(field.into());
            let error = check_proposals(
                &policy,
                "rust-worker",
                &snapshot,
                &[edit("Status"), edit(field)],
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("proposal line 2 refused"), "{field}");
            assert!(message.contains("Agent is never editable"), "{field}");
        }
    }

    #[test]
    fn strict_policy_table() {
        for (input, valid) in [
            (DEFAULT_AGENT_POLICY.to_owned(), true),
            (format!("extra = true\n{DEFAULT_AGENT_POLICY}"), false),
            (
                DEFAULT_AGENT_POLICY.replace("enabled = true", "enabled = true\nextra = true"),
                false,
            ),
            (
                DEFAULT_AGENT_POLICY.replace("\"Result\"", "\"Agent\""),
                false,
            ),
            (
                DEFAULT_AGENT_POLICY.replace("\"Result\"", "\"Status\""),
                false,
            ),
            (
                DEFAULT_AGENT_POLICY.replace("close_issue", "create_issue"),
                false,
            ),
            (
                DEFAULT_AGENT_POLICY.replace("enabled = true", "enabled = \"yes\""),
                false,
            ),
        ] {
            assert_eq!(AgentPolicy::parse(&input).is_ok(), valid, "{input}");
        }
        let policy = AgentPolicy::parse(DEFAULT_AGENT_POLICY).unwrap();
        assert!(
            policy
                .identity("unknown")
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        let disabled =
            AgentPolicy::parse(&DEFAULT_AGENT_POLICY.replace("enabled = true", "enabled = false"))
                .unwrap();
        assert!(
            disabled
                .identity("rust-worker")
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
    }

    #[test]
    fn assignment_table() {
        let policy = AgentPolicy::parse(DEFAULT_AGENT_POLICY).unwrap();
        for (name, valid) in [
            (None, true),
            (Some("rust-worker"), true),
            (Some("board-coordinator"), true),
            (Some("unknown"), false),
        ] {
            let mut snapshot = snapshot(name);
            assert_eq!(policy.normalize(&mut snapshot).is_ok(), valid);
            if valid {
                assert_eq!(snapshot.items[0].assignment.as_deref(), name);
            }
        }
        for case in [
            "wrong-type",
            "wrong-option",
            "duplicate-schema",
            "missing-schema",
            "forged-assignment",
        ] {
            let mut snapshot = snapshot(Some("rust-worker"));
            match case {
                "wrong-type" => snapshot.fields.last_mut().unwrap()["dataType"] = json!("TEXT"),
                "wrong-option" => {
                    snapshot.items[0].fields.get_mut("Agent").unwrap()["optionId"] = json!("peer")
                }
                "duplicate-schema" => snapshot
                    .fields
                    .push(snapshot.fields.last().unwrap().clone()),
                "missing-schema" => {
                    snapshot.fields.pop();
                }
                _ => snapshot.items[0].assignment = Some("board-coordinator".into()),
            }
            assert!(policy.normalize(&mut snapshot).is_err(), "{case}");
        }
    }

    #[test]
    fn proposal_table() {
        let policy = AgentPolicy::parse(DEFAULT_AGENT_POLICY).unwrap();
        for (assignment, field, reason) in [
            (Some("rust-worker"), "Status", None),
            (None, "Status", Some("not assigned")),
            (Some("board-coordinator"), "Status", Some("not assigned")),
            (Some("rust-worker"), "Agent", Some("never editable")),
            (None, "Agent", Some("never editable")),
            (Some("rust-worker"), "Priority", Some("field not permitted")),
        ] {
            let result = check_proposals(
                &policy,
                "rust-worker",
                &snapshot(assignment),
                &[edit(field)],
            );
            match reason {
                Some(reason) => assert!(format!("{:#}", result.unwrap_err()).contains(reason)),
                None => result.unwrap(),
            }
        }
        let snapshot = snapshot(Some("rust-worker"));
        let close = json!({"type":"close_issue", "target_repo":"example/intake", "issue_number":1});
        check_proposals(&policy, "rust-worker", &snapshot, &[close]).unwrap();
        let peer_snapshot = self::snapshot(Some("board-coordinator"));
        check_proposals(
            &policy,
            "board-coordinator",
            &peer_snapshot,
            &[edit("Status")],
        )
        .unwrap();
        let close = json!({"type":"close_issue", "target_repo":"example/intake", "issue_number":1});
        let error =
            check_proposals(&policy, "board-coordinator", &peer_snapshot, &[close]).unwrap_err();
        assert!(format!("{error:#}").contains("output type not permitted"));
        for (key, value) in [
            (
                "project",
                json!("https://github.com/orgs/example/projects/2"),
            ),
            ("target_repo", json!("example/peer")),
            ("content_number", json!(999)),
            ("content_type", json!("pull_request")),
            ("type", json!("create_issue")),
            ("extra", json!(true)),
            ("fields", json!({})),
            ("fields", json!({"Status":"Done", "Agent":"rust-worker"})),
        ] {
            let mut output = edit("Status");
            output[key] = value;
            assert!(
                check_proposals(&policy, "rust-worker", &snapshot, &[edit("Status"), output])
                    .is_err(),
                "{key}"
            );
        }
        for case in ["coverage", "fields", "kind", "host", "unknown", "disabled"] {
            let mut snapshot = snapshot.clone();
            let mut policy = AgentPolicy::parse(DEFAULT_AGENT_POLICY).unwrap();
            let name = match case {
                "coverage" => {
                    snapshot.coverage.complete = false;
                    "rust-worker"
                }
                "fields" => {
                    snapshot.items[0].fields_complete = false;
                    "rust-worker"
                }
                "kind" => {
                    snapshot.items[0].content_kind.clear();
                    "rust-worker"
                }
                "host" => {
                    snapshot.items[0].identity.host = "other.example".into();
                    "rust-worker"
                }
                "disabled" => {
                    policy.agents.get_mut("rust-worker").unwrap().enabled = false;
                    "rust-worker"
                }
                _ => "unknown",
            };
            assert!(
                check_proposals(&policy, name, &snapshot, &[edit("Status")]).is_err(),
                "{case}"
            );
        }
    }
}
