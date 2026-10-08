//! Read adapters and proposal lowering, deliberately no mutation client.
use anyhow::{Context, Result, ensure};
use board_core::{Capability, Intent, ItemRef, Plan, Snapshot};
use serde_json::{Value, json};
use url::Url;

/// Interpret GitHub syntax only for the explicitly selected GitHub host.
pub fn parse_github_item(input: &str, repository: Option<&str>, host: &str) -> Result<ItemRef> {
    let (repository, number) = if input.contains("://") {
        let url = Url::parse(input).context("parse GitHub issue URL")?;
        ensure!(
            url.scheme() == "https" && url.host_str() == Some(host),
            "issue URL must use the selected GitHub host and HTTPS"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "issue URL must be canonical"
        );
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        ensure!(
            parts.len() == 4 && parts[2] == "issues",
            "expected a GitHub issue URL"
        );
        (
            format!("{}/{}", parts[0], parts[1]),
            parts[3].parse().context("parse issue number")?,
        )
    } else {
        let (repo, number) = match input.rsplit_once('#') {
            Some(parts) => parts,
            None => (repository.context("bare numbers require --repo")?, input),
        };
        (repo.into(), number.parse().context("parse issue number")?)
    };
    let item = ItemRef {
        host: host.into(),
        repository,
        number,
    };
    item.validate()?;
    ensure!(
        item.repository.split('/').count() == 2,
        "GitHub repositories require OWNER/REPO"
    );
    Ok(item)
}

pub fn validate_github_project(project: &Url) -> Result<()> {
    ensure!(
        project.scheme() == "https"
            && project.host_str() == Some("github.com")
            && project.port().is_none()
            && project.username().is_empty()
            && project.password().is_none()
            && project.query().is_none()
            && project.fragment().is_none(),
        "unsupported_capability: noncanonical GitHub project"
    );
    let parts: Vec<_> = project.path().trim_matches('/').split('/').collect();
    ensure!(
        parts.len() == 4
            && ["orgs", "users"].contains(&parts[0])
            && !parts[1].is_empty()
            && parts[2] == "projects"
            && parts[3].parse::<u64>().is_ok_and(|n| n > 0),
        "invalid GitHub project URL"
    );
    Ok(())
}

pub fn github_item_url(item: &ItemRef) -> Result<String> {
    item.validate()?;
    ensure!(
        item.repository.split('/').count() == 2,
        "invalid GitHub repository"
    );
    Ok(format!(
        "https://{}/{}/issues/{}",
        item.host, item.repository, item.number
    ))
}

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
        validate_github_project(&plan.project)?;
        ensure!(plan.actions.len() <= 10, "proposal limit exceeded (10)");
        plan.actions.iter().map(|action| match &action.intent {
            Intent::SetStatus { item, value } => {
                ensure!(item.host == "github.com", "item host mismatch");
                item.validate()?;
                ensure!(item.repository.split('/').count() == 2, "invalid GitHub repository");
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
    fn github_identity_table() {
        for input in [
            "example/intake#1",
            "https://github.com/example/intake/issues/1",
        ] {
            assert_eq!(
                parse_github_item(input, None, "github.com").unwrap().number,
                1
            );
        }
        assert_eq!(
            parse_github_item("1", Some("example/intake"), "github.example")
                .unwrap()
                .host,
            "github.example"
        );
        for input in [
            "1",
            "example/intake#0",
            "https://github.com/example/intake/pull/1",
            "https://gitlab.example/example/intake/issues/1",
            "group/sub/repo#1",
        ] {
            assert!(parse_github_item(input, None, "github.com").is_err());
        }
    }

    #[test]
    fn github_project_table() {
        for (url, valid) in [
            ("https://github.com/orgs/example/projects/1", true),
            ("https://github.com/users/example/projects/1", true),
            ("https://github.com/not-a-project", false),
            ("https://forge.example/orgs/example/projects/1", false),
            ("https://github.com/orgs/example/projects/0", false),
        ] {
            assert_eq!(
                validate_github_project(&Url::parse(url).unwrap()).is_ok(),
                valid
            );
        }
    }

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
