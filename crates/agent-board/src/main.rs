mod live;

use anyhow::{Context, Result, bail, ensure};
use board_core::{Capability, Policy, Snapshot};
use board_forge::{GitHubSafeOutputs, OutputBackend, parse_github_item};
use clap::{Args, Parser, Subcommand};
use jaq_interpret::FilterT;
use serde_json::Value;
use std::{
    io::{self, Write},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    name = "agent-board",
    version,
    about = "Typed board snapshots and preview-only proposals"
)]
struct Cli {
    /// Read an offline board snapshot instead of GitHub.
    #[arg(long, global = true)]
    snapshot: Option<PathBuf>,
    /// Print selected comma-separated JSON fields.
    #[arg(long, global = true, value_name = "FIELDS")]
    json: Option<String>,
    /// Filter JSON output with a jq expression.
    #[arg(long, global = true, requires = "json")]
    jq: Option<String>,
    /// Repository OWNER/REPO for filtering or bare item numbers.
    #[arg(short = 'R', long, global = true)]
    repo: Option<String>,
    /// GitHub hostname (only github.com is supported for live reads).
    #[arg(long, global = true, default_value = board_forge::github::DEFAULT_HOST)]
    hostname: String,
    /// Organization or user owning the live project.
    #[arg(long, global = true, conflicts_with = "snapshot")]
    owner: Option<String>,
    /// Live project number, not its node ID.
    #[arg(long, global = true, conflicts_with = "snapshot")]
    project: Option<u64>,
    #[arg(long, global = true, hide = true)]
    template: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect a project's summary and schema.
    Project {
        #[command(subcommand)]
        command: Project,
    },
    /// List or inspect board items.
    Item {
        #[command(subcommand)]
        command: Item,
    },
    #[command(hide = true)]
    Ask {
        #[command(subcommand)]
        command: Ask,
    },
    #[command(hide = true)]
    Run {
        #[command(subcommand)]
        command: Run,
    },
    #[command(hide = true)]
    Request {
        #[command(subcommand)]
        command: Request,
    },
    /// Save a normalized board snapshot for offline use.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Preview deterministic board repairs; never apply them.
    Reconcile {
        #[command(subcommand)]
        command: Reconcile,
    },
    #[command(hide = true)]
    Output {
        #[command(subcommand)]
        command: Output,
    },
    /// Run the credential-bearing staging integration test.
    Test {
        #[command(subcommand)]
        command: Test,
    },
}

#[derive(Subcommand)]
enum Project {
    /// Show item counts and available fields (use --json for snapshot data).
    View,
    #[command(hide = true)]
    Init,
}

#[derive(Subcommand)]
enum Item {
    /// List board items, including closed issues by default.
    List(List),
    /// Inspect an item by URL, OWNER/REPO#N, or N with --repo.
    View { item: String },
    #[command(hide = true)]
    Add { url: String },
    #[command(hide = true)]
    Create,
    #[command(hide = true)]
    Edit { item: String },
    #[command(hide = true)]
    Close { item: String },
}

#[derive(Args)]
struct List {
    /// Filter by exact board status (for example "In Progress").
    #[arg(long)]
    status: Option<String>,
    /// Filter by priority: P0, P1 or P2.
    #[arg(long)]
    priority: Option<String>,
    /// Filter by whose turn it is (for example Coordinator).
    #[arg(long)]
    turn: Option<String>,
    /// Filter issue state: open, closed or all.
    #[arg(long, default_value = "all")]
    state: String,
    /// Maximum number of items to display.
    #[arg(short = 'L', long, default_value_t = 30)]
    limit: usize,
}

#[derive(Subcommand)]
enum Ask {
    Create {
        #[arg(long)]
        item: String,
    },
    List,
}

#[derive(Subcommand)]
enum Run {
    List,
    View { run: String },
}

#[derive(Subcommand)]
enum Request {
    Create {
        #[arg(long)]
        item: String,
    },
    View {
        issue: String,
    },
    Dispatch,
}

#[derive(Subcommand)]
enum SnapshotCommand {
    /// Write the complete normalized snapshot as JSON to stdout.
    Create,
}

#[derive(Subcommand)]
enum Reconcile {
    /// Propose repairs from a live board or offline snapshot.
    Plan {
        /// Write safe-output proposals as JSONL, not authorized mutations.
        #[arg(long)]
        emit: bool,
    },
}

#[derive(Subcommand)]
enum Output {
    Check,
    Apply,
}

#[derive(Subcommand)]
enum Test {
    /// Create and clean up scratch objects in the staging project (writes).
    Project(live::Options),
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() { 1 } else { 0 };
            let _ = error.print();
            std::process::exit(code);
        }
    };
    if let Err(error) = execute(cli) {
        eprintln!("error: {error:#}");
        std::process::exit(live::error_exit_code(&error));
    }
}

fn load(cli: &Cli) -> Result<Snapshot> {
    if cli.snapshot.is_none() {
        use board_forge::Forge;
        ensure!(
            cli.hostname == board_forge::github::DEFAULT_HOST,
            "unsupported GitHub host"
        );
        let owner = cli.owner.clone().context("live reads require --owner")?;
        let number = cli.project.context("live reads require --project")?;
        let snapshot = board_forge::github::GitHub {
            transport: board_forge::github::Client::from_env()?,
            owner,
            number,
        }
        .snapshot()?;
        if !snapshot.coverage.complete {
            eprintln!("Warning: snapshot coverage is incomplete; reconciliation is blocked.");
        }
        return Ok(snapshot);
    }
    let path = cli
        .snapshot
        .as_ref()
        .context("--snapshot FILE is required; live board reads are not implemented")?;
    let snapshot: Snapshot = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .context("decode snapshot")?;
    snapshot.validate()?;
    Ok(snapshot)
}

fn execute(cli: Cli) -> Result<()> {
    ensure!(
        cli.template.is_none(),
        "unsupported_capability: --template is not implemented"
    );
    match &cli.command {
        Command::Snapshot {
            command: SnapshotCommand::Create,
        } => render(serde_json::to_value(load(&cli)?)?, &cli),
        Command::Project {
            command: Project::View,
        } => {
            let snapshot = load(&cli)?;
            if cli.json.is_some() {
                render(serde_json::to_value(snapshot)?, &cli)
            } else {
                project_summary(&snapshot)
            }
        }
        Command::Item {
            command: Item::List(args),
        } => {
            ensure!(
                ["open", "closed", "all"].contains(&args.state.as_str()),
                "--state must be open, closed or all"
            );
            let snapshot = load(&cli)?;
            let mut items: Vec<_> = snapshot
                .items
                .iter()
                .filter(|item| {
                    (args.state == "all" || item.state == args.state)
                        && matches_filter(&item.status, &args.status)
                        && matches_filter(&item.priority, &args.priority)
                        && matches_filter(&item.turn, &args.turn)
                        && cli
                            .repo
                            .as_ref()
                            .is_none_or(|repo| *repo == item.identity.repository)
                })
                .collect();
            items.sort_by(|a, b| a.identity.cmp(&b.identity));
            items.truncate(args.limit);
            let values = items
                .iter()
                .map(|item| item_json(item))
                .collect::<Result<Vec<_>>>()?;
            if cli.json.is_some() {
                render(Value::Array(values), &cli)
            } else {
                let mut out = io::stdout().lock();
                writeln!(out, "ITEM\tSTATUS\tPRIORITY\tTURN\tTITLE")?;
                for item in items {
                    writeln!(
                        out,
                        "{}\t{}\t{}\t{}\t{}",
                        item.identity.display(),
                        label(&item.status),
                        label(&item.priority),
                        label(&item.turn),
                        item.title
                    )?;
                }
                Ok(())
            }
        }
        Command::Item {
            command: Item::View { item },
        } => {
            let snapshot = load(&cli)?;
            let identity = parse_github_item(item, cli.repo.as_deref(), &cli.hostname)?;
            let item = snapshot
                .items
                .iter()
                .find(|item| item.identity == identity)
                .context("item not found in snapshot (not evidence of forge absence)")?;
            if cli.json.is_some() {
                render(item_json(item)?, &cli)
            } else {
                writeln!(
                    io::stdout().lock(),
                    "{}: {}\nStatus: {}\nTurn: {}",
                    item.identity.display(),
                    item.title,
                    label(&item.status),
                    label(&item.turn)
                )?;
                Ok(())
            }
        }
        Command::Reconcile {
            command: Reconcile::Plan { emit },
        } => {
            let snapshot = load(&cli)?;
            // Offline scope is only a proposal; independent policy checking is mandatory.
            let policy = Policy {
                project: snapshot.project.clone(),
                repositories: snapshot
                    .items
                    .iter()
                    .map(|item| item.identity.repository.clone())
                    .collect(),
                capabilities: vec![Capability::ProjectFieldEdit, Capability::CloseIssue],
            };
            let plan = board_core::reconcile(&snapshot, &policy, &snapshot.clock)?;
            if *emit {
                ensure!(cli.json.is_none(), "--emit cannot be combined with --json");
                let mut out = io::stdout().lock();
                for output in GitHubSafeOutputs.lower(&plan)? {
                    serde_json::to_writer(&mut out, &output)?;
                    writeln!(out)?;
                }
                eprintln!(
                    "Proposed only; not applied. Independent bounds/check/apply support is required."
                );
                Ok(())
            } else {
                eprintln!("Preview only; not applied.");
                render(serde_json::to_value(plan)?, &cli)
            }
        }
        Command::Test {
            command: Test::Project(options),
        } => {
            ensure!(cli.snapshot.is_none(), "live fixture cannot use --snapshot");
            live::run(
                options,
                cli.owner.as_deref(),
                cli.repo.as_deref(),
                cli.project,
            )
        }
        _ => bail!(
            "unsupported_capability: command is not implemented; use gh project item-add, gh project item-edit, gh issue create or gh issue close for manual changes; agent-board only reads and proposes repairs"
        ),
    }
}

fn project_summary(snapshot: &Snapshot) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "Project: {}", snapshot.project)?;
    writeln!(out, "Items: {}", snapshot.items.len())?;
    writeln!(
        out,
        "Coverage: {} (observed {})",
        if snapshot.coverage.complete {
            "complete"
        } else {
            "incomplete"
        },
        snapshot.coverage.observed_at
    )?;
    let mut counts = std::collections::BTreeMap::new();
    for item in &snapshot.items {
        *counts.entry(label(&item.status)).or_insert(0_usize) += 1;
    }
    writeln!(out, "STATUS\tITEMS")?;
    for (status, count) in counts {
        writeln!(out, "{status}\t{count}")?;
    }
    writeln!(out, "FIELD\tOPTIONS")?;
    for name in ["Status", "Priority", "Turn"] {
        let field = snapshot
            .fields
            .iter()
            .find(|field| field["name"].as_str() == Some(name));
        let description = match field {
            None => "missing (or schema unavailable in snapshot)".to_owned(),
            Some(field) => field["options"]
                .as_array()
                .filter(|options| !options.is_empty())
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option["name"].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "no single-select options".into()),
        };
        writeln!(out, "{name}\t{description}")?;
    }
    Ok(())
}

fn label<T: serde::Serialize>(value: &Option<T>) -> String {
    value
        .as_ref()
        .and_then(|v| serde_json::to_value(v).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".into())
}

fn matches_filter<T: serde::Serialize>(value: &Option<T>, filter: &Option<String>) -> bool {
    filter.as_ref().is_none_or(|filter| label(value) == *filter)
}

fn item_json(item: &board_core::Item) -> Result<Value> {
    let mut value = serde_json::to_value(item)?;
    value["number"] = item.identity.number.into();
    value["repository"] = item.identity.repository.clone().into();
    let url = board_forge::github_item_url(&item.identity)?;
    value["url"] = if item.content_kind == "pull_request" {
        url.replace("/issues/", "/pull/")
    } else {
        url
    }
    .into();
    Ok(value)
}

fn select(value: &Value, fields: &[&str]) -> Result<Value> {
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .map(|v| select(v, fields))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array);
    }
    let object = value
        .as_object()
        .context("field selection requires an object")?;
    let mut selected = serde_json::Map::new();
    for field in fields {
        selected.insert(
            (*field).into(),
            object
                .get(*field)
                .with_context(|| format!("unknown JSON field: {field}"))?
                .clone(),
        );
    }
    Ok(Value::Object(selected))
}

fn render(mut value: Value, cli: &Cli) -> Result<()> {
    if let Some(fields) = &cli.json {
        let available: &[&str] = if matches!(cli.command, Command::Item { .. }) {
            &[
                "identity",
                "title",
                "state",
                "status",
                "priority",
                "turn",
                "fields_complete",
                "evidence",
                "number",
                "repository",
                "url",
                "content_kind",
                "labels",
                "assignees",
                "timestamps",
                "fields",
            ]
        } else {
            &[
                "schema",
                "project",
                "actions",
                "diagnostics",
                "clock",
                "coverage",
                "items",
                "fields",
            ]
        };
        for field in fields.split(',') {
            ensure!(
                available.contains(&field),
                "unknown JSON field: {field}; available fields: {}",
                available.join(",")
            );
        }
        value = select(&value, &fields.split(',').collect::<Vec<_>>())?;
    }
    let mut out = io::stdout().lock();
    if let Some(expression) = &cli.jq {
        let mut context = jaq_interpret::ParseCtx::new(Vec::new());
        context.insert_natives(jaq_core::core());
        context.insert_defs(jaq_std::std());
        let (parsed, errors) = jaq_parse::parse(expression, jaq_parse::main());
        ensure!(errors.is_empty(), "invalid --jq expression: {errors:?}");
        let filter = context.compile(parsed.context("empty --jq expression")?);
        ensure!(
            context.errs.is_empty(),
            "invalid --jq expression: unknown function or invalid binding"
        );
        let inputs = jaq_interpret::RcIter::new(std::iter::empty());
        for result in filter.run((jaq_interpret::Ctx::new([], &inputs), value.into())) {
            let result = result.map_err(|error| anyhow::anyhow!("jq: {error}"))?;
            let value: Value = result.into();
            if let Some(string) = value.as_str() {
                writeln!(out, "{string}")?;
            } else {
                writeln!(out, "{value}")?;
            }
        }
    } else {
        serde_json::to_writer_pretty(&mut out, &value)?;
        writeln!(out)?;
    }
    Ok(())
}
