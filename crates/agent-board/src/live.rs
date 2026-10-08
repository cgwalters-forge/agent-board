//! Opt-in staging fixture. Only locally held creation IDs authorize cleanup.
use anyhow::{Context, Result, bail, ensure};
use board_core::{Capability, Policy, Status};
use board_forge::{
    Forge,
    github::{AuthenticationRequired, Client, GitHub, Transport},
};
use clap::Args;
use serde_json::{Value, json};

const DELETE: &str =
    "mutation($id:ID!){deleteProjectV2(input:{projectId:$id}){deletedProjectV2Id}}";
const CLOSE: &str = "mutation($id:ID!){closeIssue(input:{issueId:$id}){issue{id state}}}";

#[derive(Args)]
pub struct Options {
    /// Sweeping remains disabled: names and descriptions cannot authorize deletion.
    #[arg(long)]
    sweep: bool,
}

pub fn error_exit_code(error: &anyhow::Error) -> i32 {
    if error.downcast_ref::<AuthenticationRequired>().is_some() {
        4
    } else {
        1
    }
}

pub fn run(options: &Options, owner: Option<&str>, repo: Option<&str>) -> Result<()> {
    ensure!(
        !options.sweep,
        "unsupported_capability: sweeping requires independently trusted creation receipts; use manual recovery"
    );
    let owner = owner.context("test project requires --owner")?;
    let repo = repo.context("test project requires --repo OWNER/REPO")?;
    ensure!(
        owner == "cgwalters-forge-stage",
        "live fixture owner must be cgwalters-forge-stage"
    );
    let (repo_owner, name) = repo.split_once('/').context("expected OWNER/REPO")?;
    ensure!(
        repo_owner == owner && !name.is_empty() && !name.contains('/'),
        "scratch repository must belong to the staging owner"
    );
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate fixture nonce"))?;
    let title = format!(
        "agent-board-ci-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    lifecycle(&Client::from_env()?, owner, repo, &title)
}

fn string(value: &Value, pointer: &str) -> Result<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("missing fixture response {pointer}; mutation may be ambiguous"))
}

fn lifecycle(t: &impl Transport, owner: &str, repo: &str, title: &str) -> Result<()> {
    let (_, name) = repo.split_once('/').context("expected OWNER/REPO")?;
    let scope = t.graphql("query($owner:String!,$repo:String!){organization(login:$owner){id} repository(owner:$owner,name:$repo){id nameWithOwner}}", json!({"owner":owner,"repo":name}))?;
    ensure!(
        scope["repository"]["nameWithOwner"].as_str() == Some(repo),
        "scratch repository identity mismatch"
    );
    let organization = string(&scope, "/organization/id")?;
    let repository = string(&scope, "/repository/id")?;
    let created = t.graphql("mutation($owner:ID!,$title:String!){createProjectV2(input:{ownerId:$owner,title:$title}){projectV2{id number}}}", json!({"owner":organization,"title":title}))
        .context("create staging project: the token needs Organization Projects read/write permission; fine-grained token support must be verified live")?;
    let project_id = string(&created, "/createProjectV2/projectV2/id")?;
    eprintln!("Created fixture project {project_id}; cleanup uses only this in-memory ID.");
    let mut issue_id = None;
    let result = (|| -> Result<()> {
        let number = created["createProjectV2"]["projectV2"]["number"]
            .as_u64()
            .context("missing created project number")?;
        let issue = t.graphql("mutation($repo:ID!,$title:String!){createIssue(input:{repositoryId:$repo,title:$title,body:\"Disposable agent-board live fixture\"}){issue{id number}}}", json!({"repo":repository,"title":title}))?;
        issue_id = Some(string(&issue, "/createIssue/issue/id")?);
        eprintln!(
            "Created scratch issue {}; cleanup will close it.",
            issue_id.as_deref().context("missing local issue ID")?
        );
        let issue_number = issue["createIssue"]["issue"]["number"]
            .as_u64()
            .context("missing created issue number")?;
        let added = t.graphql("mutation($project:ID!,$issue:ID!){addProjectV2ItemById(input:{projectId:$project,contentId:$issue}){item{id}}}", json!({"project":project_id,"issue":issue_id}))?;
        let item_id = string(&added, "/addProjectV2ItemById/item/id")?;
        let field = t.graphql("mutation($project:ID!){createProjectV2Field(input:{projectId:$project,dataType:TEXT,name:\"Fixture nonce\"}){projectV2Field{... on ProjectV2Field{id}}}}", json!({"project":project_id}))?;
        let field_id = string(&field, "/createProjectV2Field/projectV2Field/id")?;
        t.graphql("mutation($project:ID!,$item:ID!,$field:ID!,$text:String!){updateProjectV2ItemFieldValue(input:{projectId:$project,itemId:$item,fieldId:$field,value:{text:$text}}){projectV2Item{id}}}", json!({"project":project_id,"item":item_id,"field":field_id,"text":title}))?;
        struct Borrowed<'a, T>(&'a T);
        impl<T: Transport> Transport for Borrowed<'_, T> {
            fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
                self.0.graphql(query, variables)
            }
        }
        let snapshot = GitHub {
            transport: Borrowed(t),
            owner: owner.into(),
            number,
        }
        .snapshot()?;
        ensure!(
            snapshot.coverage.complete && snapshot.items.len() == 1,
            "live snapshot did not return complete fixture membership"
        );
        let item = &snapshot.items[0];
        ensure!(
            item.identity.repository == repo
                && item.identity.number == issue_number
                && item.title == title
                && item.state == "open"
                && item.fields_complete
                && item.content_kind == "issue",
            "live issue readback mismatch"
        );
        ensure!(
            item.fields
                .get("Fixture nonce")
                .and_then(|v| v["text"].as_str())
                == Some(title),
            "live field readback mismatch"
        );
        ensure!(
            snapshot
                .fields
                .iter()
                .any(|f| f["name"].as_str() == Some("Fixture nonce")),
            "live schema readback mismatch"
        );
        ensure!(
            item.labels.is_empty()
                && item.assignees.is_empty()
                && item.timestamps.contains_key("createdAt")
                && item.timestamps.contains_key("updatedAt"),
            "live issue facts mismatch"
        );
        let policy = Policy {
            project: snapshot.project.clone(),
            repositories: vec![repo.into()],
            capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
        };
        let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock)?;
        ensure!(
            plan.actions.len() == 1
                && matches!(
                    plan.actions[0].intent,
                    board_core::Intent::SetStatus {
                        value: Status::Triage,
                        ..
                    }
                ),
            "live reconcile plan mismatch"
        );
        Ok(())
    })();
    // Attempt both cleanups even if either fails. Never read a project name or marker
    // to decide what can be deleted; the ID comes only from this create response.
    let close = if let Some(id) = &issue_id {
        t.graphql(CLOSE, json!({"id":id})).and_then(|v| {
            ensure!(
                v["closeIssue"]["issue"]["id"].as_str() == Some(id)
                    && v["closeIssue"]["issue"]["state"] == "CLOSED",
                "issue cleanup receipt mismatch"
            );
            Ok(())
        })
    } else {
        Ok(())
    };
    let delete = t.graphql(DELETE, json!({"id":project_id})).and_then(|v| {
        ensure!(
            v["deleteProjectV2"]["deletedProjectV2Id"].as_str() == Some(&project_id),
            "project cleanup receipt mismatch"
        );
        Ok(())
    });
    if close.is_err() || delete.is_err() {
        bail!(
            "fixture cleanup failed; manual recovery: project {project_id}, issue {}; issue cleanup: {}; project cleanup: {}; fixture: {}",
            issue_id.as_deref().unwrap_or("not created or ambiguous"),
            close
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into()),
            delete
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into()),
            result
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "ok".into())
        );
    }
    result.context("live fixture failed; cleanup succeeded")?;
    println!(
        "Fixture schema, membership, issue facts, field, reconcile plan and cleanup verified."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Recorded {
        calls: RefCell<Vec<String>>,
        fail: usize,
    }

    impl Transport for Recorded {
        fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
            let mut calls = self.calls.borrow_mut();
            calls.push(query.into());
            if calls.len() == self.fail {
                bail!("recorded failure");
            }
            if query == DELETE {
                assert_eq!(variables["id"], "P_created");
                return Ok(json!({"deleteProjectV2":{"deletedProjectV2Id":"P_created"}}));
            }
            if query == CLOSE {
                assert_eq!(variables["id"], "I_created");
                return Ok(json!({"closeIssue":{"issue":{"id":"I_created","state":"CLOSED"}}}));
            }
            if query.contains("organization(login") {
                return Ok(
                    json!({"organization":{"id":"O_stage"},"repository":{"id":"R_scratch","nameWithOwner":"cgwalters-forge-stage/board-test"}}),
                );
            }
            if query.contains("createProjectV2(") {
                return Ok(json!({"createProjectV2":{"projectV2":{"id":"P_created","number":1}}}));
            }
            if query.contains("createIssue(") {
                return Ok(json!({"createIssue":{"issue":{"id":"I_created","number":1}}}));
            }
            if query.contains("addProjectV2ItemById(") {
                return Ok(json!({"addProjectV2ItemById":{"item":{"id":"M_created"}}}));
            }
            if query.contains("createProjectV2Field(") {
                return Ok(json!({"createProjectV2Field":{"projectV2Field":{"id":"F_created"}}}));
            }
            if query.contains("updateProjectV2ItemFieldValue(") {
                return Ok(
                    json!({"updateProjectV2ItemFieldValue":{"projectV2Item":{"id":"M_created"}}}),
                );
            }
            if query.contains("repositoryOwner(") {
                return Ok(
                    json!({"repositoryOwner":{"projectV2":{"id":"P_created","url":"https://github.com/orgs/cgwalters-forge-stage/projects/1"}}}),
                );
            }
            let (key, nodes) = if query.contains("fields(first") {
                (
                    "fields",
                    json!([{"id":"F_created","name":"Fixture nonce","dataType":"TEXT"},{"id":"F_status","name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"todo","name":"Todo"}]}]),
                )
            } else if query.contains("items(first") {
                (
                    "items",
                    json!([{"id":"M_created","content":{"__typename":"Issue","id":"I_created","number":1,"title":"nonce","state":"OPEN","createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null,"repository":{"nameWithOwner":"cgwalters-forge-stage/board-test"}}}]),
                )
            } else if query.contains("fieldValues(first") {
                (
                    "fieldValues",
                    json!([{"__typename":"ProjectV2ItemFieldTextValue","text":"nonce","field":{"name":"Fixture nonce"}}]),
                )
            } else if query.contains("labels(first") {
                ("labels", json!([]))
            } else {
                ("assignees", json!([]))
            };
            Ok(
                json!({"node":{key:{"nodes":nodes,"pageInfo":{"hasNextPage":false,"endCursor":null}}}}),
            )
        }
    }

    #[test]
    fn lifecycle_failure_table() {
        for fail in 0..=15 {
            let t = Recorded {
                calls: RefCell::new(vec![]),
                fail,
            };
            let result = lifecycle(
                &t,
                "cgwalters-forge-stage",
                "cgwalters-forge-stage/board-test",
                "nonce",
            );
            assert_eq!(result.is_ok(), fail == 0 || fail > 14, "failure at {fail}");
            let calls = t.calls.borrow();
            if fail != 1 && fail != 2 {
                assert!(calls.iter().any(|q| q == DELETE));
            }
            if fail == 0 || fail >= 4 {
                assert!(calls.iter().any(|q| q == CLOSE));
            }
        }
    }
}
