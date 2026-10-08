//! Read adapters and proposal lowering, deliberately no mutation client.
use anyhow::{Result, ensure};
use board_core::{Capability, Intent, Plan, Snapshot};
use serde_json::{Value, json};

pub trait Forge {
    fn snapshot(&self) -> Result<Snapshot>;
    fn capabilities(&self) -> Result<Vec<Capability>>;
}

pub trait OutputBackend {
    fn lower(&self, plan: &Plan) -> Result<Vec<Value>>;
}

pub struct GitHubSafeOutputs;

impl OutputBackend for GitHubSafeOutputs {
    fn lower(&self, plan: &Plan) -> Result<Vec<Value>> {
        ensure!(
            plan.schema == board_core::PLAN_SCHEMA,
            "unsupported plan schema"
        );
        ensure!(
            plan.project.host_str() == Some("github.com"),
            "unsupported_capability: non-GitHub project"
        );
        ensure!(plan.actions.len() <= 10, "proposal limit exceeded (10)");
        plan.actions.iter().map(|action| match &action.intent {
            Intent::SetStatus { item, value } => {
                ensure!(item.host == "github.com", "item host mismatch");
                Ok(json!({"type":"update_project", "project":plan.project, "content_type":"issue", "content_number":item.number, "target_repo":item.repository, "fields":{"Status":value}}))
            }
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_core::{Policy, reconcile};

    #[test]
    fn exact_proposal() {
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec!["example/intake".into()],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let plan = reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
        assert_eq!(
            GitHubSafeOutputs.lower(&plan).unwrap(),
            vec![
                json!({"type":"update_project","project":snapshot.project,"content_type":"issue","content_number":1,"target_repo":"example/intake","fields":{"Status":"Triage"}})
            ]
        );
    }
}
