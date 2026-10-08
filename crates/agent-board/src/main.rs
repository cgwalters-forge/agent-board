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
    #[arg(long, global = true)]
    snapshot: Option<PathBuf>,
    #[arg(long, global = true, value_name = "FIELDS")]
    json: Option<String>,
    #[arg(long, global = true, requires = "json")]
    jq: Option<String>,
    #[arg(short = 'R', long, global = true)]
    repo: Option<String>,
    #[arg(long, global = true, default_value = "github.com")]
    hostname: String,
    #[arg(long, global = true)]
    template: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Project {
        #[command(subcommand)]
        command: Project,
    },
    Item {
        #[command(subcommand)]
        command: Item,
    },
    Ask {
        #[command(subcommand)]
        command: Ask,
    },
    Run {
        #[command(subcommand)]
        command: Run,
    },
    Request {
        #[command(subcommand)]
        command: Request,
    },
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    Reconcile {
        #[command(subcommand)]
        command: Reconcile,
    },
    Output {
        #[command(subcommand)]
        command: Output,
    },
    Test {
        #[command(subcommand)]
        command: Test,
    },
}

#[derive(Subcommand)]
enum Project {
    View,
    Init,
}

#[derive(Subcommand)]
enum Item {
    List(List),
    View { item: String },
    Add { url: String },
    Create,
    Edit { item: String },
    Close { item: String },
}

#[derive(Args)]
struct List {
    #[arg(long)]
    status: Option<String>,
    #[arg(long)]
    priority: Option<String>,
    #[arg(long)]
    turn: Option<String>,
    #[arg(long, default_value = "open")]
    state: String,
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
    Create,
}

#[derive(Subcommand)]
enum Reconcile {
    Plan {
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
            live::run(options)
        }
        _ => bail!("unsupported_capability: command not implemented in step 1"),
    }
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
    value["url"] = board_forge::github_item_url(&item.identity)?.into();
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
            ]
        } else {
            &["schema", "project", "actions", "diagnostics"]
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
