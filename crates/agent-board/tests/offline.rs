use assert_cmd::Command;
use serde_json::Value;

fn command() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("agent-board"))
}

fn fixture() -> String {
    format!("{}/../../fixtures/board.json", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn proposal_check_cli_table() {
    let snapshot = format!(
        "{}/../../fixtures/assigned-board.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let valid = serde_json::json!({"type":"update_project", "project":"https://github.com/orgs/example/projects/1", "target_repo":"example/intake", "content_type":"issue", "content_number":1, "fields":{"Status":"Done"}});
    for (identity, number, field, reason) in [
        ("rust-worker", 1, "Status", None),
        ("rust-worker", 2, "Status", Some("not assigned")),
        ("rust-worker", 3, "Status", Some("not assigned")),
        ("rust-worker", 1, "Agent", Some("never editable")),
        ("rust-worker", 1, "agent", Some("never editable")),
        ("rust-worker", 1, "AGENT", Some("never editable")),
        ("rust-worker", 1, "aGeNt", Some("never editable")),
        ("rust-worker", 3, "Agent", Some("never editable")),
        ("rust-worker", 1, "Priority", Some("field not permitted")),
        ("unknown", 1, "Status", Some("unknown agent identity")),
    ] {
        let mut output = valid.clone();
        output["content_number"] = serde_json::json!(number);
        output["fields"] = serde_json::json!({field:"rust-worker"});
        let result = command()
            .args([
                "check",
                "proposals",
                "--as",
                identity,
                "--snapshot",
                &snapshot,
            ])
            .write_stdin(format!("{output}\n"))
            .assert();
        if let Some(reason) = reason {
            let output = result.code(1).get_output().clone();
            assert!(String::from_utf8(output.stderr).unwrap().contains(reason));
        } else {
            result.success().stdout("");
        }
    }
    for (input, reason) in [
        (format!("{valid}\nnot json\n"), "decode proposal line 2"),
        (
            format!("{valid}\n{{\"type\":\"create_issue\"}}\n"),
            "proposal line 2 refused",
        ),
    ] {
        let output = command()
            .args([
                "check",
                "proposals",
                "--as",
                "rust-worker",
                "--snapshot",
                &snapshot,
            ])
            .write_stdin(input)
            .assert()
            .code(1)
            .get_output()
            .clone();
        assert!(String::from_utf8(output.stderr).unwrap().contains(reason));
    }
    command()
        .args(["check", "proposals", "--as", "rust-worker"])
        .write_stdin("")
        .assert()
        .code(1);
    let output = command()
        .args(["snapshot", "create", "--snapshot", &snapshot])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let normalized: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(normalized["items"][0]["assignment"], "rust-worker");
    assert_eq!(normalized["items"][1]["assignment"], "board-coordinator");
    assert!(normalized["items"][2]["assignment"].is_null());
}

#[test]
fn custom_policy_snapshot_and_check_cli() {
    // Honor TMPDIR, allowing sandbox runners to keep scratch files under home.
    let directory = std::env::temp_dir().join(format!("agent-board-policy-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let policy = directory.join("agents.toml");
    let snapshot = directory.join("board.json");
    let proposals = directory.join("outputs.jsonl");
    let original = include_str!("../../../fixtures/assigned-board.json")
        .replace("rust-worker", "custom-worker");
    std::fs::write(&snapshot, original).unwrap();
    std::fs::write(
        &policy,
        board_core::DEFAULT_AGENT_POLICY.replace("rust-worker", "custom-worker"),
    )
    .unwrap();
    let args = [
        "--policy",
        policy.to_str().unwrap(),
        "--snapshot",
        snapshot.to_str().unwrap(),
    ];
    let output = command()
        .args(["snapshot", "create"])
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let normalized: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(normalized["items"][0]["assignment"], "custom-worker");
    std::fs::write(&snapshot, output).unwrap();
    std::fs::write(
        &proposals,
        "{\"type\":\"close_issue\",\"target_repo\":\"example/intake\",\"issue_number\":1}\n",
    )
    .unwrap();
    command()
        .args([
            "check",
            "proposals",
            "--as",
            "custom-worker",
            "--proposals",
            proposals.to_str().unwrap(),
        ])
        .args(args)
        .assert()
        .success();
    std::fs::write(
        &policy,
        board_core::DEFAULT_AGENT_POLICY
            .replace("rust-worker", "custom-worker")
            .replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    let output = command()
        .args([
            "check",
            "proposals",
            "--as",
            "custom-worker",
            "--proposals",
            proposals.to_str().unwrap(),
        ])
        .args(args)
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("disabled agent identity")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dispatch_preview_cli_refuses_ineligible_items_and_profiles() {
    for extra in [vec![], vec!["--kind", "review"], vec!["--json", "argv"]] {
        command()
            .args([
                "request",
                "dispatch",
                "--item",
                "example/intake#1",
                "--caller",
                "example/runners",
                "--snapshot",
                &fixture(),
            ])
            .args(extra)
            .assert()
            .code(1);
    }
    command()
        .args(["request", "dispatch", "--help"])
        .assert()
        .success();
    command()
        .args(["run", "record", "--help"])
        .assert()
        .success();
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
    assert_eq!(reasons, ["closed_issue_not_done"]);
    assert!(
        plan["diagnostics"]["example/intake#1"]
            .as_str()
            .unwrap()
            .contains("needs_review")
    );
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
