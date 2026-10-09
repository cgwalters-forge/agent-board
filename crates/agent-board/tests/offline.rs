use assert_cmd::Command;
use serde_json::Value;

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
}

fn fixture() -> String {
    format!("{}/../../fixtures/board.json", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn offline_table() {
    for (args, expected) in [
        (
            vec![
                "item",
                "view",
                "example/intake#1",
                "--json",
                "turn",
                "--jq",
                ".turn",
            ],
            "Coordinator\n",
        ),
        (
            vec![
                "item",
                "view",
                "https://github.com/example/intake/issues/1",
                "--json",
                "number",
                "--jq",
                ".number",
            ],
            "1\n",
        ),
        (
            vec![
                "item",
                "view",
                "1",
                "-R",
                "example/intake",
                "--json",
                "number",
                "--jq",
                ".number",
            ],
            "1\n",
        ),
        (
            vec![
                "item",
                "list",
                "--status",
                "Triage",
                "--json",
                "number",
                "--jq",
                ".[0].number",
            ],
            "2\n",
        ),
        (
            vec![
                "reconcile",
                "plan",
                "--json",
                "actions",
                "--jq",
                ".actions[].reason",
            ],
            "missing_status\n",
        ),
    ] {
        let output = command()
            .args(args)
            .args(["--snapshot", &fixture()])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
}

#[test]
fn emitted_proposal() {
    let output = command()
        .args(["reconcile", "plan", "--snapshot", &fixture(), "--emit"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["type"], "update_project");
    assert_eq!(value["fields"]["Status"], "Triage");
}

#[test]
fn snapshot_and_project_views() {
    for args in [["snapshot", "create"], ["project", "view"]] {
        let output = command()
            .args(args)
            .args(["--snapshot", &fixture()])
            .args(["--json", "schema,project,clock,coverage,items,fields"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let snapshot: board_core::Snapshot = serde_json::from_slice(&output).unwrap();
        snapshot.validate().unwrap();
        assert_eq!(snapshot.items.len(), 2);
    }
}

#[test]
fn first_run_help_and_summary() {
    for (args, present, absent) in [
        (
            vec!["--help"],
            "Preview deterministic board repairs",
            "  ask ",
        ),
        (
            vec!["item", "--help"],
            "including closed issues",
            "  create ",
        ),
        (
            vec!["item", "list", "--help"],
            "[default: all]",
            "--template",
        ),
        (vec!["item", "view", "--help"], "OWNER/REPO#N", "--template"),
        (
            vec!["snapshot", "create", "--help"],
            "JSON to stdout",
            "--template",
        ),
    ] {
        let output = command()
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(present), "{text}");
        assert!(!text.contains(absent), "{text}");
    }
    let output = command()
        .args(["project", "view", "--snapshot", &fixture()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    for expected in [
        "Project: https://github.com/orgs/example/projects/1",
        "Items: 2",
        "Triage\t1",
        "Turn\tmissing",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    assert!(!text.contains("board-snapshot/v1"));
    let output = command()
        .args(["item", "add", "https://github.com/example/intake/issues/1"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("gh project item-add, gh project item-edit")
    );
}

#[test]
fn list_includes_closed_items_by_default() {
    let path = format!(
        "{}/../../fixtures/terminal-drift.json",
        env!("CARGO_MANIFEST_DIR")
    );
    for (filter, expected) in [
        (None, 2),
        (Some("open"), 1),
        (Some("closed"), 1),
        (Some("all"), 2),
    ] {
        let mut cmd = command();
        cmd.args(["item", "list", "--snapshot", &path, "--json", "number"]);
        if let Some(state) = filter {
            cmd.args(["--state", state]);
        }
        let output = cmd.assert().success().get_output().stdout.clone();
        let items: Vec<Value> = serde_json::from_slice(&output).unwrap();
        assert_eq!(items.len(), expected);
    }
}

#[test]
fn project_schema_summary_table() {
    for (field, expected) in [
        (
            serde_json::json!({"name":"Status","options":[{"name":"Todo"},{"name":"Done"}]}),
            "Todo, Done",
        ),
        (
            serde_json::json!({"name":"Status","options":[]}),
            "no single-select options",
        ),
        (
            serde_json::json!({"name":"Status","dataType":"TEXT"}),
            "no single-select options",
        ),
    ] {
        let mut snapshot: Value =
            serde_json::from_str(include_str!("../../../fixtures/board.json")).unwrap();
        snapshot["fields"] = serde_json::json!([field]);
        snapshot["items"] = serde_json::json!([]);
        snapshot["coverage"]["complete"] = false.into();
        let output = command()
            .args(["project", "view", "--snapshot", "/dev/stdin"])
            .write_stdin(serde_json::to_vec(&snapshot).unwrap())
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let text = String::from_utf8(output).unwrap();
        for expected in [
            format!("Status\t{expected}"),
            "Items: 0".into(),
            "Coverage: incomplete".into(),
        ] {
            assert!(text.contains(&expected), "{text}");
        }
    }
}

#[test]
fn live_reads_require_authentication() {
    for args in [
        vec!["snapshot", "create"],
        vec!["project", "view"],
        vec!["item", "list"],
        vec!["item", "view", "example/intake#1"],
        vec!["reconcile", "plan"],
    ] {
        command()
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args(args)
            .args(["--owner", "example", "--project", "1"])
            .assert()
            .code(4);
    }
}

#[test]
fn terminal_drift_preview_and_emit() {
    let fixture = format!(
        "{}/../../fixtures/terminal-drift.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = command()
        .args(["reconcile", "plan", "--snapshot", &fixture])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let plan: Value = serde_json::from_slice(&output).unwrap();
    let mut reasons: Vec<_> = plan["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["reason"].as_str().unwrap())
        .collect();
    reasons.sort();
    assert_eq!(reasons, ["closed_issue_not_done", "done_issue_open"]);
    let output = command()
        .args(["reconcile", "plan", "--snapshot", &fixture, "--emit"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let mut proposals: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    proposals.sort_by_key(|value| value["type"].as_str().unwrap().to_owned());
    assert_eq!(
        proposals,
        [
            serde_json::json!({"type":"close_issue", "target_repo":"example/intake", "issue_number":1}),
            serde_json::json!({"type":"update_project", "project":"https://github.com/orgs/example/projects/1", "content_type":"issue", "content_number":2, "target_repo":"example/intake", "fields":{"Status":"Done"}}),
        ]
    );
}

#[test]
fn errors_and_help() {
    for args in [
        vec!["project", "view"],
        vec!["item", "list"],
        vec!["item", "list", "--unknown"],
        vec!["item", "view", "1", "--snapshot", &fixture()],
        vec![
            "item",
            "view",
            "https://forge.example/example/intake/issues/1",
            "--snapshot",
            &fixture(),
        ],
    ] {
        command().args(args).assert().code(1);
    }
    for noun in [
        "project",
        "item",
        "ask",
        "run",
        "request",
        "snapshot",
        "reconcile",
        "output",
        "test",
    ] {
        command().args([noun, "--help"]).assert().success();
    }
    Command::new(assert_cmd::cargo::cargo_bin!("gh-agent-board"))
        .args(["item", "list", "--snapshot", &fixture()])
        .assert()
        .success();
}

#[test]
fn machine_errors_table() {
    for args in [
        vec!["item", "list", "--status", "Done", "--json", "nonexistent"],
        vec![
            "item",
            "list",
            "--json",
            "number",
            "--jq",
            "unknown_function",
        ],
        vec!["item", "list", "--json", "number", "--jq", "["],
        vec!["reconcile", "plan", "--emit", "--json", "actions"],
        vec!["item", "list", "--template", "{{.title}}"],
    ] {
        command()
            .args(args)
            .args(["--snapshot", &fixture()])
            .assert()
            .code(1);
    }
    for (mode, code) in [
        (vec!["--project", "2"], 4),
        (vec!["--create-project"], 4),
        (vec![], 1),
        (vec!["--project", "0"], 1),
        (vec!["--project", "2", "--create-project"], 1),
    ] {
        command()
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .args([
                "test",
                "project",
                "--owner",
                "cgwalters-forge-stage",
                "--repo",
                "cgwalters-forge-stage/board-test",
            ])
            .args(mode)
            .assert()
            .code(code);
    }
    command()
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .args([
            "test",
            "project",
            "--owner",
            "cgwalters-forge-stage",
            "--repo",
            "cgwalters-forge-stage/board-test",
            "--sweep",
        ])
        .assert()
        .code(1);
}
