//! GitHub Projects v2 read transport. Connections are exhausted, never inferred complete.
use super::Forge;
use anyhow::{Context, Result, ensure};
use board_core::{Capability, Coverage, Evidence, Item, ItemRef, Snapshot};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

pub const DEFAULT_HOST: &str = "github.com";

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
            parts.len() == 4 && ["issues", "pull"].contains(&parts[2]),
            "expected a GitHub issue or pull request URL"
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
            && project.host_str() == Some(DEFAULT_HOST)
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

#[derive(Debug)]
pub struct AuthenticationRequired;

impl std::fmt::Display for AuthenticationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "authentication required: provide a valid GH_TOKEN or GITHUB_TOKEN"
        )
    }
}

impl std::error::Error for AuthenticationRequired {}

pub trait Transport {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value>;
}

// Shared across every connection and per-item read in one snapshot. Exhaustion
// returns an error, never a partial snapshot that could yield proposals.
const SNAPSHOT_REQUEST_LIMIT: usize = 1_000;

struct ReadBudget<'a, T> {
    transport: &'a T,
    remaining: std::cell::Cell<usize>,
}

impl<T: Transport> Transport for ReadBudget<'_, T> {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let remaining = self.remaining.get();
        ensure!(
            remaining > 0,
            "GitHub snapshot request budget exhausted; no snapshot produced"
        );
        self.remaining.set(remaining - 1);
        self.transport.graphql(query, variables)
    }
}

pub struct Client {
    token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn from_env() -> Result<Self> {
        let token = select_token(
            std::env::var("GH_TOKEN").ok(),
            std::env::var("GITHUB_TOKEN").ok(),
        )?;
        Ok(Self {
            token,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(30)))
                .build()
                .into(),
        })
    }
}

fn select_token(gh: Option<String>, github: Option<String>) -> Result<String> {
    gh.filter(|s| !s.is_empty())
        .or_else(|| github.filter(|s| !s.is_empty()))
        .ok_or_else(|| AuthenticationRequired.into())
}

fn transport_error(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(401) => AuthenticationRequired.into(),
        ureq::Error::StatusCode(403) => anyhow::anyhow!(
            "GitHub denied access: check Organization Projects read/write permission, repository Issues read/write permission, organization approval and rate limits"
        ),
        ureq::Error::StatusCode(code) => anyhow::anyhow!("GitHub HTTP status {code}"),
        _ => anyhow::anyhow!("GitHub transport failed"),
    }
}

fn response_data(response: Value) -> Result<Value> {
    if response["errors"].as_array().is_some_and(|errors| {
        errors
            .iter()
            .any(|e| e["type"] == "UNAUTHORIZED" || e["extensions"]["code"] == "UNAUTHENTICATED")
    }) {
        return Err(AuthenticationRequired.into());
    }
    ensure!(
        response
            .get("errors")
            .is_none_or(|e| e.is_null() || e.as_array().is_some_and(Vec::is_empty)),
        "GitHub GraphQL denied or failed the operation: check Organization Projects permission and repository Issues permission; response may be partial"
    );
    response
        .get("data")
        .filter(|v| v.is_object())
        .cloned()
        .context("missing GraphQL data")
}

impl Transport for Client {
    fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let response: Value = self
            .agent
            .post("https://api.github.com/graphql")
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("User-Agent", "agent-board")
            .send_json(json!({"query":query,"variables":variables}))
            .map_err(transport_error)?
            .body_mut()
            .read_json()
            .context("decode GitHub GraphQL response")?;
        response_data(response)
    }
}

pub struct GitHub<T> {
    pub transport: T,
    pub owner: String,
    pub number: u64,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("missing GitHub {key}"))
}

fn connection(
    t: &impl Transport,
    query: &str,
    variables: Value,
    pointer: &str,
) -> Result<(Vec<Value>, bool)> {
    let mut variables = variables;
    let mut nodes = Vec::new();
    let mut cursors = BTreeSet::new();
    let mut complete = true;
    loop {
        let data = t.graphql(query, variables.clone())?;
        let conn = data.pointer(pointer).context("missing GitHub connection")?;
        let page = conn["nodes"]
            .as_array()
            .context("missing connection nodes")?;
        for node in page {
            if node.is_null() {
                complete = false;
            } else {
                nodes.push(node.clone());
            }
        }
        match conn["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => return Ok((nodes, complete)),
            Some(true) => {
                let cursor = text(&conn["pageInfo"], "endCursor")?;
                ensure!(
                    cursors.insert(cursor.to_owned()),
                    "GitHub pagination cursor repeated"
                );
                variables["cursor"] = cursor.into();
            }
            None => return Ok((nodes, false)),
        }
    }
}

fn typed_field(
    fields: &[Value],
    values: &BTreeMap<String, Value>,
    name: &str,
) -> Result<Option<Value>> {
    let schema = fields.iter().find(|f| f["name"].as_str() == Some(name));
    if schema.is_none() && name != "Status" && !values.contains_key(name) {
        return Ok(None);
    }
    let schema = schema.context("missing typed field schema")?;
    ensure!(
        schema["dataType"] == "SINGLE_SELECT",
        "typed field is not single-select"
    );
    if let Some(value) = values.get(name) {
        ensure!(
            value["__typename"] == "ProjectV2ItemFieldSingleSelectValue",
            "incompatible typed field value"
        );
        let option = text(value, "optionId")?;
        let selected = text(value, "name")?;
        ensure!(
            schema["options"].as_array().is_some_and(|opts| opts
                .iter()
                .any(|o| o["id"] == option && o["name"] == selected)),
            "unknown typed field option"
        );
        Ok(Some(selected.into()))
    } else {
        Ok(None)
    }
}

impl<T: Transport> Forge for GitHub<T> {
    fn capabilities(&self) -> Result<Vec<Capability>> {
        // Read access never establishes mutation authority.
        Ok(vec![])
    }

    fn snapshot(&self) -> Result<Snapshot> {
        ensure!(self.number > 0, "project number must be positive");
        let transport = ReadBudget {
            transport: &self.transport,
            remaining: std::cell::Cell::new(SNAPSHOT_REQUEST_LIMIT),
        };
        let data = transport.graphql("query($owner:String!,$number:Int!){repositoryOwner(login:$owner){... on Organization{projectV2(number:$number){id url}} ... on User{projectV2(number:$number){id url}}}}", json!({"owner":self.owner,"number":self.number}))?;
        let project = &data["repositoryOwner"]["projectV2"];
        let id = text(project, "id")?;
        let clock = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)?;
        let (fields, mut complete) = connection(
            &transport,
            "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{fields(first:100,after:$cursor){nodes{... on ProjectV2Field{id name dataType} ... on ProjectV2SingleSelectField{id name dataType options{id name}} ... on ProjectV2IterationField{id name dataType configuration{iterations{id title startDate duration} completedIterations{id title startDate duration}}}} pageInfo{hasNextPage endCursor}}}}}",
            json!({"id":id}),
            "/node/fields",
        )?;
        let (members, members_complete) = connection(
            &transport,
            "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2{items(first:100,after:$cursor){nodes{id content{__typename ... on Issue{id number title state createdAt updatedAt closedAt repository{nameWithOwner}} ... on PullRequest{id number title state createdAt updatedAt closedAt mergedAt repository{nameWithOwner}}}} pageInfo{hasNextPage endCursor}}}}}",
            json!({"id":id}),
            "/node/items",
        )?;
        complete &= members_complete;
        let mut items = Vec::new();
        for member in members {
            let content = &member["content"];
            let kind = content["__typename"].as_str();
            if !matches!(kind, Some("Issue" | "PullRequest")) {
                complete = false;
                continue;
            }
            let content_id = text(content, "id")?;
            let native_state = text(content, "state")?;
            ensure!(
                matches!(native_state, "OPEN" | "CLOSED")
                    || (kind == Some("PullRequest") && native_state == "MERGED"),
                "unknown GitHub content state"
            );
            let (values, values_complete) = connection(
                &transport,
                "query($id:ID!,$cursor:String){node(id:$id){... on ProjectV2Item{fieldValues(first:100,after:$cursor){nodes{__typename ... on ProjectV2ItemFieldSingleSelectValue{name optionId field{... on ProjectV2SingleSelectField{name}}} ... on ProjectV2ItemFieldTextValue{text field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldNumberValue{number field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldDateValue{date field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldIterationValue{title iterationId field{... on ProjectV2IterationField{name}}} ... on ProjectV2ItemFieldRepositoryValue{repository{id nameWithOwner} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldMilestoneValue{milestone{id title state dueOn} field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldLabelValue{id field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldUserValue{id field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldPullRequestValue{id field{... on ProjectV2Field{name}}} ... on ProjectV2ItemFieldReviewerValue{id field{... on ProjectV2Field{name}}}} pageInfo{hasNextPage endCursor}}}}}",
                json!({"id":text(&member,"id")?}),
                "/node/fieldValues",
            )?;
            let mut item_fields = BTreeMap::new();
            let mut values_supported = true;
            for mut value in values {
                let nested = match value["__typename"].as_str() {
                    Some("ProjectV2ItemFieldLabelValue") => Some(("labels", "id name")),
                    Some("ProjectV2ItemFieldUserValue") => Some(("users", "id login")),
                    Some("ProjectV2ItemFieldPullRequestValue") => {
                        Some(("pullRequests", "id number url title state"))
                    }
                    Some("ProjectV2ItemFieldReviewerValue") => Some((
                        "reviewers",
                        "__typename ... on User{id login} ... on Team{id name}",
                    )),
                    _ => None,
                };
                if let Some((field, selection)) = nested {
                    let ty = text(&value, "__typename")?;
                    let query = format!(
                        "query($id:ID!,$cursor:String){{node(id:$id){{... on {ty}{{{field}(first:100,after:$cursor){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
                    );
                    let (nodes, full) = connection(
                        &transport,
                        &query,
                        json!({"id":text(&value,"id")?}),
                        &format!("/node/{field}"),
                    )?;
                    values_supported &= full;
                    value[field] = nodes.into();
                }
                if let Some(name) = value["field"]["name"].as_str() {
                    item_fields.insert(name.to_owned(), value);
                } else {
                    // An unimplemented or redacted value must not become an
                    // observed null. Keep its type and block this item's repairs.
                    values_supported = false;
                    item_fields.insert(format!("unsupported:{}", item_fields.len()), value);
                }
            }
            let mut facts_complete = values_complete
                && values_supported
                && content["createdAt"].is_string()
                && content["updatedAt"].is_string()
                && content.get("closedAt").is_some()
                && (kind != Some("PullRequest") || content.get("mergedAt").is_some());
            let mut facts = BTreeMap::new();
            for (field, selection) in [("labels", "name"), ("assignees", "login")] {
                let query = format!(
                    "query($id:ID!,$cursor:String){{node(id:$id){{... on Issue{{{field}(first:100,after:$cursor){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}}} ... on PullRequest{{{field}(first:100,after:$cursor){{nodes{{{selection}}} pageInfo{{hasNextPage endCursor}}}}}}}}}}"
                );
                let (nodes, full) = connection(
                    &transport,
                    &query,
                    json!({"id":content_id}),
                    &format!("/node/{field}"),
                )?;
                facts_complete &= full;
                facts.insert(
                    field,
                    nodes
                        .iter()
                        .map(|n| text(n, selection).map(str::to_owned))
                        .collect::<Result<Vec<_>>>()?,
                );
            }
            let mut typed_complete = facts_complete;
            let status = typed_field(&fields, &item_fields, "Status")
                .and_then(|v| {
                    v.map(serde_json::from_value)
                        .transpose()
                        .map_err(Into::into)
                })
                .unwrap_or_else(|_| {
                    typed_complete = false;
                    None
                });
            let priority = typed_field(&fields, &item_fields, "Priority")
                .and_then(|v| {
                    v.map(serde_json::from_value)
                        .transpose()
                        .map_err(Into::into)
                })
                .unwrap_or_else(|_| {
                    typed_complete = false;
                    None
                });
            let turn = typed_field(&fields, &item_fields, "Turn")
                .and_then(|v| {
                    v.map(serde_json::from_value)
                        .transpose()
                        .map_err(Into::into)
                })
                .unwrap_or_else(|_| {
                    typed_complete = false;
                    None
                });
            complete &= typed_complete;
            let identity = ItemRef {
                host: DEFAULT_HOST.into(),
                repository: text(&content["repository"], "nameWithOwner")?.into(),
                number: content["number"]
                    .as_u64()
                    .context("missing content number")?,
            };
            let url = format!(
                "https://{}/{}/{}/{}",
                DEFAULT_HOST,
                identity.repository,
                if kind == Some("Issue") {
                    "issues"
                } else {
                    "pull"
                },
                identity.number
            )
            .parse()?;
            items.push(Item {
                identity,
                title: text(content, "title")?.into(),
                state: if native_state == "OPEN" {
                    "open"
                } else {
                    "closed"
                }
                .into(),
                status,
                priority,
                turn,
                fields_complete: typed_complete,
                evidence: vec![Evidence {
                    url,
                    observed_at: clock.clone(),
                    provenance: "GitHub GraphQL".into(),
                }],
                content_kind: if kind == Some("Issue") {
                    "issue"
                } else {
                    "pull_request"
                }
                .into(),
                labels: facts.remove("labels").unwrap_or_default(),
                assignees: facts.remove("assignees").unwrap_or_default(),
                timestamps: ["createdAt", "updatedAt", "closedAt", "mergedAt"]
                    .into_iter()
                    .filter_map(|k| {
                        content
                            .get(k)
                            .map(|v| (k.into(), v.as_str().map(str::to_owned)))
                    })
                    .collect(),
                fields: item_fields,
            });
        }
        let snapshot = Snapshot {
            schema: board_core::SNAPSHOT_SCHEMA.into(),
            project: text(project, "url")?.parse()?,
            clock: clock.clone(),
            coverage: Coverage {
                complete,
                observed_at: clock,
            },
            items,
            fields,
        };
        validate_github_project(&snapshot.project)?;
        snapshot.validate()?;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct Recorded {
        responses: RefCell<VecDeque<Value>>,
        variables: RefCell<Vec<Value>>,
    }

    impl Transport for Recorded {
        fn graphql(&self, _: &str, variables: Value) -> Result<Value> {
            self.variables.borrow_mut().push(variables);
            self.responses
                .borrow_mut()
                .pop_front()
                .context("unexpected request")
        }
    }

    fn page(nodes: Value, next: bool, cursor: Value) -> Value {
        json!({"node":{"connection":{"nodes":nodes,"pageInfo":{"hasNextPage":next,"endCursor":cursor}}}})
    }

    #[test]
    fn authentication_table() {
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
                Some(s) => assert_eq!(result.unwrap(), s),
                None => assert!(result.unwrap_err().is::<AuthenticationRequired>()),
            }
        }
        for (status, authentication) in [(401, true), (403, false), (429, false), (500, false)] {
            assert_eq!(
                transport_error(ureq::Error::StatusCode(status))
                    .context("request")
                    .is::<AuthenticationRequired>(),
                authentication
            );
        }
        for (response, authentication) in [
            (
                json!({"errors":[{"type":"UNAUTHORIZED","message":"not echoed"}]}),
                true,
            ),
            (
                json!({"errors":[{"extensions":{"code":"UNAUTHENTICATED"}}]}),
                true,
            ),
            (
                json!({"data":{"partial":true},"errors":[{"type":"FORBIDDEN","message":"not echoed"}]}),
                false,
            ),
        ] {
            let error = response_data(response).unwrap_err();
            assert_eq!(error.is::<AuthenticationRequired>(), authentication);
            assert!(!error.to_string().contains("not echoed"));
        }
    }

    #[test]
    fn pagination_table() {
        for (responses, expected_complete, count, requests) in [
            (
                vec![page(json!([{"id":"one"}]), false, Value::Null)],
                true,
                1,
                1,
            ),
            (
                vec![
                    page(json!([{"id":"one"}]), true, json!("next")),
                    page(json!([{"id":"two"}]), false, Value::Null),
                ],
                true,
                2,
                2,
            ),
            (
                vec![page(json!([{"id":"one"},null]), false, Value::Null)],
                false,
                1,
                1,
            ),
            (
                vec![
                    page(json!([null]), true, json!("next")),
                    page(json!([{"id":"two"}]), false, Value::Null),
                ],
                false,
                1,
                2,
            ),
            (
                vec![json!({"node":{"connection":{"nodes":[]}}})],
                false,
                0,
                1,
            ),
        ] {
            let t = Recorded {
                responses: RefCell::new(responses.into()),
                variables: RefCell::new(vec![]),
            };
            let (nodes, complete) =
                connection(&t, "recorded", json!({"id":"project"}), "/node/connection").unwrap();
            assert_eq!(complete, expected_complete);
            assert_eq!(nodes.len(), count);
            assert_eq!(t.variables.borrow().len(), requests);
            if requests == 2 {
                assert_eq!(t.variables.borrow()[1]["cursor"], "next");
            }
        }
    }

    #[test]
    fn aggregate_request_budget_table() {
        for (pages, limit, succeeds) in [(2, 2, true), (3, 2, false)] {
            let t = Recorded {
                responses: RefCell::new(
                    (0..pages)
                        .map(|i| page(json!([]), i + 1 < pages, json!(format!("cursor-{i}"))))
                        .collect(),
                ),
                variables: RefCell::new(vec![]),
            };
            let budget = ReadBudget {
                transport: &t,
                remaining: std::cell::Cell::new(limit),
            };
            let result = connection(&budget, "recorded", json!({}), "/node/connection");
            assert_eq!(result.is_ok(), succeeds);
            if !succeeds {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("request budget exhausted")
                );
            }
            assert_eq!(t.variables.borrow().len(), limit);
            // A second connection cannot reset the snapshot's aggregate budget.
            assert!(connection(&budget, "another", json!({}), "/node/connection").is_err());
            assert_eq!(t.variables.borrow().len(), limit);
        }
    }

    #[test]
    fn snapshot_unique_cursor_exhaustion() {
        let mut responses = vec![json!({"repositoryOwner":{"projectV2":{"id":"P"}}})];
        for i in 1..SNAPSHOT_REQUEST_LIMIT {
            responses.push(json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":format!("cursor-{i}")}}}}));
        }
        let github = GitHub {
            transport: Recorded {
                responses: RefCell::new(responses.into()),
                variables: RefCell::new(vec![]),
            },
            owner: "example".into(),
            number: 1,
        };
        let error = github.snapshot().unwrap_err();
        assert!(error.to_string().contains("request budget exhausted"));
        assert_eq!(
            github.transport.variables.borrow().len(),
            SNAPSHOT_REQUEST_LIMIT
        );
    }

    #[test]
    fn malformed_pagination_table() {
        for responses in [
            vec![page(json!([]), true, Value::Null)],
            vec![
                page(json!([]), true, json!("same")),
                page(json!([]), true, json!("same")),
            ],
            vec![json!({"node":null})],
        ] {
            let t = Recorded {
                responses: RefCell::new(responses.into()),
                variables: RefCell::new(vec![]),
            };
            assert!(connection(&t, "recorded", json!({}), "/node/connection").is_err());
        }
    }

    #[test]
    fn redacted_membership_blocks_reconcile() {
        let t = Recorded { responses: RefCell::new(vec![
            json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
            json!({"node":{"fields":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}),
            json!({"node":{"items":{"nodes":[{"id":"M","content":null}],"pageInfo":{"hasNextPage":false}}}}),
        ].into()),variables:RefCell::new(vec![]) };
        let snapshot = GitHub {
            transport: t,
            owner: "example".into(),
            number: 1,
        }
        .snapshot()
        .unwrap();
        assert!(!snapshot.coverage.complete);
        let policy = board_core::Policy {
            project: snapshot.project.clone(),
            repositories: vec!["example/intake".into()],
            capabilities: vec![Capability::ProjectFieldEdit],
        };
        let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
        assert!(plan.actions.is_empty());
        assert!(plan.diagnostics.contains_key("coverage"));
    }

    #[test]
    fn typed_schema_table() {
        let schema = json!({"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"done","name":"Done"}]});
        let value = json!({"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done"});
        for (field, selected, valid) in [
            (schema.clone(), Some(value.clone()), true),
            (schema.clone(), None, true),
            (
                json!({"name":"Status","dataType":"TEXT"}),
                Some(json!({"__typename":"ProjectV2ItemFieldTextValue","text":"Done"})),
                false,
            ),
            (
                schema.clone(),
                Some(
                    json!({"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"unknown"}),
                ),
                false,
            ),
            (
                schema,
                Some(json!({"__typename":"ProjectV2ItemFieldTextValue","text":"Done"})),
                false,
            ),
        ] {
            let values = selected
                .map(|v| BTreeMap::from([("Status".into(), v)]))
                .unwrap_or_default();
            assert_eq!(typed_field(&[field], &values, "Status").is_ok(), valid);
        }
        assert!(typed_field(&[], &BTreeMap::new(), "Status").is_err());
    }

    #[test]
    fn issue_and_pull_request_facts_table() {
        for (kind, state) in [
            ("Issue", "OPEN"),
            ("Issue", "CLOSED"),
            ("PullRequest", "OPEN"),
            ("PullRequest", "MERGED"),
        ] {
            let conn = |name: &str, nodes: Value| json!({"node":{name:{"nodes":nodes,"pageInfo":{"hasNextPage":false}}}});
            let t = Recorded { responses: RefCell::new(vec![
                json!({"repositoryOwner":{"projectV2":{"id":"P","url":"https://github.com/orgs/example/projects/1"}}}),
                conn("fields",json!([{"name":"Status","dataType":"SINGLE_SELECT","options":[{"id":"done","name":"Done"}]}])),
                conn("items",json!([{"id":"M","content":{"__typename":kind,"id":"I","number":7,"title":"recorded","state":state,"repository":{"nameWithOwner":"example/intake"},"createdAt":"2026-10-08T00:00:00Z","updatedAt":"2026-10-08T00:00:00Z","closedAt":null,"mergedAt":null}}])),
                conn("fieldValues",json!([{"__typename":"ProjectV2ItemFieldSingleSelectValue","name":"Done","optionId":"done","field":{"name":"Status"}}])),
                conn("labels",json!([{"name":"bug"}])),conn("assignees",json!([{"login":"human"}])),
            ].into()),variables:RefCell::new(vec![]) };
            let snapshot = GitHub {
                transport: t,
                owner: "example".into(),
                number: 1,
            }
            .snapshot()
            .unwrap();
            assert!(snapshot.coverage.complete);
            let item = &snapshot.items[0];
            assert!(item.fields_complete);
            assert_eq!(item.labels, ["bug"]);
            assert_eq!(item.assignees, ["human"]);
            assert_eq!(item.state, if state == "OPEN" { "open" } else { "closed" });
            let policy = board_core::Policy {
                project: snapshot.project.clone(),
                repositories: vec!["example/intake".into()],
                capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
            };
            let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock).unwrap();
            assert_eq!(
                plan.actions.len(),
                usize::from(kind == "Issue" && state == "OPEN")
            );
        }
    }
}
