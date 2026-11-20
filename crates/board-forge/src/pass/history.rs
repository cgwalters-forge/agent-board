//! The pass history (`holds.passes`): what each of the last passes did,
//! published as the bot's own comment beside the hold snapshot for the
//! control app's Status panel (review `src/github/passes.ts`), and apart
//! from it, so that a pass whose stage failed, or whose holds could not be
//! published, leaves a record too. It is built from the pass's own decision
//! sites, never from its log: each stage's outcome from the failures counted
//! while it ran ([`Sections`]), each start from the dispatch answer
//! ([`Start`]), each item that did not start from its hold ([`NoStart`]),
//! and each pool's pace from the usage snapshot the pass read, with that
//! reading's own clock. It schedules nothing and is no authority for a
//! write.
//!
//! The threat the option stops is a write added on upgrade and a pass's
//! error text shown without the caller's opt-in. The bounds keep a reader's
//! document finite ([`fit`]), and [`redact`] keeps inference, broker and
//! tailnet addresses out of a comment everyone with the private
//! repository's read access sees.

use super::*;

pub const SCHEMA: &str = "agent-board/passes/v1";
pub const MARKER: &str = "<!-- agent-board-passes v1 -->";
/// The passes kept, newest first, and for how long.
pub(super) const RETENTION: usize = 10;
const RETENTION_DAYS: i64 = 7;
/// The JSON's size past which details go, oldest first ([`fit`]).
pub(super) const MAX_BYTES: usize = 48 * 1024;
/// A pass's sections, starts and no-start reasons listed, the rest counted
/// in its `omitted`.
const MAX_SECTIONS: usize = 16;
pub(super) const MAX_STARTS: usize = 20;
const MAX_NO_START: usize = 20;
/// The pools and the runs in flight listed, the rest counted.
const MAX_POOLS: usize = 16;
const MAX_RUNS: usize = 50;
/// A failed section's error: one line of at most this many characters.
const MAX_ERROR_CHARS: usize = 200;
/// What [`redact`] puts for an address.
const REDACTED: &str = "<address>";
/// Name suffixes of private networks, redacted as addresses are.
const PRIVATE_SUFFIXES: &[&str] = &[".ts.net", ".local", ".internal", ".lan", ".home.arpa"];

// ---- Sections ----

/// How a stage ended, as the reader names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Ok,
    Failed,
    /// Never entered: the pass stopped before it, or it was not configured.
    Skipped,
    /// Entered, with a read its share or the budget refused, a step the
    /// budget deferred, or the rate-limit breaker tripped.
    CutShort,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
            Outcome::CutShort => "cut-short",
        }
    }
}

/// A stage's outcome, and a failed one's first failed operation, as the
/// pass worded it, redacted when recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Section {
    pub name: String,
    pub outcome: Outcome,
    pub error: Option<String>,
}

/// The report's counts when a stage began, which its end is read against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Marks {
    pub failures: usize,
    /// The failed operations listed, whose next is a failed stage's error.
    pub operations: usize,
    pub deferred: usize,
    /// Reads the budget refused and requests the breaker did not send.
    pub refused: usize,
}

/// The stages' outcomes, as [`Report::enter_section`] marks them: each
/// stage is judged when the next begins or the pass ends.
#[derive(Debug, Default)]
pub(super) struct Sections {
    done: Vec<Section>,
    open: Option<(String, Marks)>,
}

impl Sections {
    /// Ends the stage under way by `marks`, the report's counts now, with
    /// `errors` its failed operations and `cut` whether the stage's share
    /// refused a read, and begins `next`.
    pub(super) fn enter(&mut self, next: Option<&str>, marks: Marks, cut: bool, errors: &[String]) {
        if let Some((name, at)) = self.open.take() {
            let (outcome, error) = if marks.failures > at.failures {
                (Outcome::Failed, errors.get(at.operations).cloned())
            } else if cut || marks.deferred > at.deferred || marks.refused > at.refused {
                (Outcome::CutShort, None)
            } else {
                (Outcome::Ok, None)
            };
            self.done.push(Section {
                name,
                outcome,
                error,
            });
        }
        self.open = next.map(|name| (name.to_owned(), marks));
    }

    /// The sections in [`Stage::ALL`]'s order, a stage never entered
    /// `skipped`; one under way is not judged yet.
    pub(super) fn all(&self) -> Vec<Section> {
        Stage::ALL
            .iter()
            .map(|stage| {
                let name = section_name(*stage);
                (self.done.iter().find(|s| s.name == name).cloned()).unwrap_or(Section {
                    name,
                    outcome: Outcome::Skipped,
                    error: None,
                })
            })
            .collect()
    }
}

/// `stage`'s section name, a token: `pull-requests` for `pull requests`.
pub(super) fn section_name(stage: Stage) -> String {
    stage.name().replace(' ', "-")
}

// ---- Starts ----

/// What the pass knows of a run it asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StartState {
    /// GitHub accepted the dispatch, whether or not it named the run.
    Confirmed,
    /// The dispatch was sent and its answer lost, or was an error GitHub
    /// may have acted on: a run may be under way.
    Uncertain,
    /// A preview's, never sent.
    Preview,
}

/// The state of the start a dispatch asked for, by GitHub's `answer`:
/// confirmed by a 2xx, none for a refusal, whether a 4xx or the answers
/// the client gives up on (rate limited, or not authenticated), and
/// uncertain for the rest: a lost answer, or a 5xx GitHub may have acted on.
pub(super) fn start_state(answer: &Result<(u16, Value)>) -> Option<StartState> {
    let refused = |error: &anyhow::Error| {
        error.downcast_ref::<RateLimited>().is_some()
            || error
                .downcast_ref::<crate::github::AuthenticationRequired>()
                .is_some()
    };
    match answer {
        Ok((200..=299, _)) => Some(StartState::Confirmed),
        Ok((400..=499, _)) => None,
        Err(error) if refused(error) => None,
        _ => Some(StartState::Uncertain),
    }
}

impl StartState {
    fn name(self) -> &'static str {
        match self {
            StartState::Confirmed => "confirmed",
            StartState::Uncertain => "uncertain",
            StartState::Preview => "preview",
        }
    }
}

/// A start the pass asked for, or in a preview would have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Start {
    state: StartState,
    item: String,
    kind: String,
    pool: Option<String>,
    /// The caller, `OWNER/REPO`, and the run's id there, where GitHub named it.
    run: Option<(String, u64)>,
}

impl Start {
    /// The start of `kind` on `repository#number` in `state`, with its run
    /// where the answer named one; none where the reader would refuse it.
    pub(super) fn new(
        state: StartState,
        (repository, number): (&str, u64),
        kind: &str,
        pool: Option<&str>,
        run: Option<(&str, u64)>,
    ) -> Option<Self> {
        if !view::named(repository) || number == 0 {
            return None;
        }
        let run = match run {
            Some((caller, id)) if view::named(caller) && id > 0 => Some((caller.to_owned(), id)),
            _ => None,
        };
        Some(Self {
            state,
            item: format!("{repository}#{number}"),
            kind: view::token(&kind.replace(' ', "-"))?,
            pool: pool.and_then(view::token),
            run,
        })
    }

    fn json(&self) -> Value {
        json!({
            "state": self.state.name(), "item": self.item, "kind": self.kind, "pool": self.pool,
            "run": self.run.as_ref().map(|(repository, id)| json!({"repository": repository, "id": id})),
        })
    }
}

// ---- No start ----

/// Why an item, or every new run, did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NoStart {
    reason: &'static str,
    item: Option<String>,
    cap: Option<&'static str>,
    hold: Option<&'static str>,
}

impl NoStart {
    fn json(&self) -> Value {
        json!({"reason": self.reason, "item": self.item, "until": null, "cap": self.cap, "hold": self.hold})
    }
}

/// The pass's `holds` as no-start reasons, in their order: those of every
/// new run first, `capped` by `wip`, `pools` or `budget` ([`view::CAPS`]),
/// then each item `held` by its hold's kind, one entry per hold; and where
/// stage 4 was `idle` with a slot free, `no-ready-work` first of all. A
/// hold's reason is never copied: it may name a spend amount. No reason
/// has an `until`: the pass records no re-plan time.
pub(super) fn no_starts(holds: &[Entry], idle: bool) -> Vec<NoStart> {
    let global = |kind: HoldKind| match view::CAPS.contains(&kind) {
        true => NoStart {
            reason: "capped",
            item: None,
            cap: Some(kind.name()),
            hold: None,
        },
        false => NoStart {
            reason: "held",
            item: None,
            cap: None,
            hold: Some(kind.name()),
        },
    };
    let held = |item: &ItemRef, kind: HoldKind| NoStart {
        reason: "held",
        item: Some(item.display()),
        cap: None,
        hold: Some(kind.name()),
    };
    let mut out = Vec::new();
    if idle {
        out.push(NoStart {
            reason: "no-ready-work",
            item: None,
            cap: None,
            hold: None,
        });
    }
    out.extend(holds.iter().filter_map(|entry| match &entry.scope {
        Scope::Pass => Some(global(entry.hold.kind)),
        _ => None,
    }));
    out.extend(holds.iter().filter_map(|entry| match &entry.scope {
        Scope::Pass => None,
        Scope::Item(item) | Scope::Epic(item) | Scope::Under(item, _) => {
            Some(held(item, entry.hold.kind))
        }
    }));
    out
}

// ---- Redaction ----

/// `text`'s first line, without control characters, at most
/// [`MAX_ERROR_CHARS`], with every word that is a URL, an IP address, a
/// `host:port` or a private network name replaced by [`REDACTED`], and
/// then each of `hosts` (the configured broker and budget endpoints)
/// wherever it appears, whatever its case: an error names what the pass
/// could not reach. The control app redacts again; this is what leaves
/// the pass.
pub(super) fn redact(text: &str, hosts: &[&str]) -> String {
    let line: String = text
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let words: Vec<String> = line
        .split(' ')
        .map(|word| {
            // The brackets of an IPv6 address stay on the word.
            let bare =
                word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '[' && c != ']');
            match address(bare) {
                true => word.replacen(bare, REDACTED, 1),
                false => word.to_owned(),
            }
        })
        .collect();
    let mut line = words.join(" ");
    for host in hosts.iter().filter(|h| !h.is_empty()) {
        line = replace_ignoring_case(&line, host, REDACTED);
    }
    let mut out: String = line.chars().take(MAX_ERROR_CHARS).collect();
    if out.chars().count() < line.chars().count() {
        out.pop();
        out.push('…');
    }
    match out.is_empty() {
        true => "error".into(),
        false => out,
    }
}

/// `text` with every occurrence of `from`, in any ASCII case, replaced by
/// `to`.
fn replace_ignoring_case(text: &str, from: &str, to: &str) -> String {
    let (lower, needle) = (text.to_ascii_lowercase(), from.to_ascii_lowercase());
    let mut out = String::with_capacity(text.len());
    let mut rest = 0;
    while let Some(at) = lower[rest..].find(&needle) {
        out.push_str(&text[rest..rest + at]);
        out.push_str(to);
        rest += at + needle.len();
    }
    out.push_str(&text[rest..]);
    out
}

/// Whether `word` is an address: a URL, an IPv4 or IPv6 address, a
/// `host:port`, or a name on a private network.
fn address(word: &str) -> bool {
    if word.contains("://") {
        return true;
    }
    // IPv6, bare or `[…]:port`: hex and at least two colons, as a tailnet
    // address is.
    let hex = |c: char| c.is_ascii_hexdigit() || ":.[]".contains(c);
    if word.matches(':').count() >= 2 && word.chars().all(hex) {
        return true;
    }
    let (host, port) = match word.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
            (host, true)
        }
        _ => (word, false),
    };
    let octets: Vec<&str> = host.split('.').collect();
    let ipv4 = octets.len() == 4
        && octets
            .iter()
            .all(|o| !o.is_empty() && o.len() <= 3 && o.bytes().all(|b| b.is_ascii_digit()));
    let name = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    let lower = host.to_ascii_lowercase();
    ipv4 || (port && name) || PRIVATE_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

// ---- The document ----

/// This pass, as [`record`] writes it.
pub(super) struct Pass<'a> {
    /// The Actions run it is, `OWNER/REPO`, id and attempt, or none.
    pub run: Option<(&'a str, u64, u64)>,
    /// Its clock: the pass reads no other, so it both started and finished
    /// at it.
    pub at: &'a str,
    /// `actions`, `preview` or `local` ([`super::Mode`]).
    pub mode: &'a str,
    pub sections: &'a [Section],
    pub starts: &'a [Start],
    pub no_start: &'a [NoStart],
    /// Stage 4's candidates: read, unread and ready to start, or none where
    /// it did not read the board.
    pub candidates: Option<(usize, usize, usize)>,
    /// The endpoints [`redact`] takes out of error text.
    pub hosts: &'a [&'a str],
}

/// `pass`'s record: `failure` where a stage failed before stage 4 ran,
/// `partial` where one failed or was cut short, else `success`, with its
/// sections, starts and no-start reasons each within their bound, the rest
/// counted in `omitted`.
pub(super) fn record(pass: &Pass) -> Value {
    let schedule = section_name(Stage::Schedule);
    let failed = pass.sections.iter().any(|s| s.outcome == Outcome::Failed);
    let cut = pass.sections.iter().any(|s| s.outcome == Outcome::CutShort);
    let scheduled =
        (pass.sections.iter()).any(|s| s.name == schedule && s.outcome != Outcome::Skipped);
    let result = match (failed, cut) {
        (true, _) if !scheduled => "failure",
        (true, _) | (_, true) => "partial",
        _ => "success",
    };
    let sections: Vec<Value> = (pass.sections.iter().take(MAX_SECTIONS))
        .map(|s| {
            let error = match s.outcome {
                Outcome::Failed => Some(redact(s.error.as_deref().unwrap_or_default(), pass.hosts)),
                _ => None,
            };
            json!({"name": s.name, "outcome": s.outcome.name(), "error": error})
        })
        .collect();
    let starts: Vec<Value> = pass
        .starts
        .iter()
        .take(MAX_STARTS)
        .map(Start::json)
        .collect();
    let no_start: Vec<Value> = (pass.no_start.iter().take(MAX_NO_START))
        .map(NoStart::json)
        .collect();
    json!({
        "run": pass.run.map(|(repository, id, attempt)| {
            json!({"repository": repository, "id": id, "attempt": attempt})
        }),
        "started_at": pass.at,
        "finished_at": pass.at,
        "mode": pass.mode,
        "result": result,
        "sections": sections,
        "starts": starts,
        "no_start": no_start,
        "candidates": pass.candidates.map(|(read, unread, ready)| {
            json!({"read": read, "unread": unread, "ready": ready})
        }),
        "omitted": {
            "sections": pass.sections.len() - sections.len(),
            "starts": pass.starts.len() - starts.len(),
            "no_start": pass.no_start.len() - no_start.len(),
        },
    })
}

/// Each pool of the usage snapshot the pass paced by, by name: its own
/// reading's clock, its use and allowance in percent, its reset and whether
/// it is held, each null where the snapshot has none. `complete` where every
/// one of the `configured` pools has an entry. Ready work is not counted by
/// pool, so `ready` is null. Null where the pass read no snapshot.
pub(super) fn pools(snapshot: Option<&Value>, configured: &[String]) -> Value {
    let Some(snapshot) = snapshot else {
        return Value::Null;
    };
    let percent = |v: &Value| {
        let p = v
            .as_f64()
            .filter(|p| p.is_finite() && (0.0..=1000.0).contains(p))?;
        Some((p * 10.0).round() / 10.0)
    };
    let entries = snapshot["pools"].as_object();
    let mut entries: Vec<_> = entries.into_iter().flatten().collect();
    entries.sort_by_key(|(name, _)| name.as_str());
    let named: Vec<_> = (entries.iter())
        .filter_map(|(name, entry)| Some((view::token(name)?, *entry)))
        .collect();
    let items: Vec<Value> = (named.iter().take(MAX_POOLS))
        .map(|(name, entry)| {
            json!({
                "pool": name,
                "observed_at": time(&entry["observed_at"]),
                "used_percent": percent(&entry["used_percent"]),
                "allowed_percent": percent(&entry["allowed_percent"]),
                "resets_at": time(&entry["resets_at"]),
                "held": entry["hold"].as_bool(),
                "ready": null,
            })
        })
        .collect();
    let complete = configured
        .iter()
        .all(|pool| entries.iter().any(|(name, _)| *name == pool));
    json!({
        "observed_at": time(&snapshot["updated_at"]),
        "complete": complete,
        "omitted": entries.len() - items.len(),
        "items": items,
    })
}

/// The runs in flight the pass listed at `at`, each with its start time as
/// `since`, at most [`MAX_RUNS`]; null where it listed none. Pull requests
/// in a merge queue or waiting for review are not exported yet: `prs` is
/// empty, and `complete` speaks of the runs.
pub(super) fn in_flight(at: &str, flights: Option<&[view::Run]>) -> Value {
    let Some(flights) = flights else {
        return Value::Null;
    };
    let runs: Vec<Value> = (flights.iter().take(MAX_RUNS))
        .map(|run| {
            let mut out = run.json();
            let since = out.as_object_mut().and_then(|o| o.remove("started_at"));
            out["since"] = since.unwrap_or(Value::Null);
            out
        })
        .collect();
    json!({
        "observed_at": at, "complete": true, "omitted": flights.len() - runs.len(),
        "runs": runs, "prs": [],
    })
}

/// `v` where it is an RFC 3339 time, else null.
fn time(v: &Value) -> Value {
    let valid = v
        .as_str()
        .filter(|t| OffsetDateTime::parse(t, &Rfc3339).is_ok());
    valid.map_or(Value::Null, Into::into)
}

/// Whether `record`, a published record, has a record's shape, so that it
/// may be carried and cut ([`cut`]): its times, its three lists and its
/// omission counts. One a person edited into another shape, or an older
/// producer's, is left out rather than refused by the reader for a week.
fn shaped(record: &Value) -> bool {
    ["started_at", "finished_at"]
        .iter()
        .all(|key| !time(&record[key]).is_null())
        && ["sections", "starts", "no_start"]
            .iter()
            .all(|key| record[key].is_array())
        && record["omitted"].is_object()
}

/// The document published at `at`: this `pass`'s record, then those of the
/// `previous` document (the published one's JSON) that have a record's
/// shape ([`shaped`]) and finished within [`RETENTION_DAYS`] before `now`,
/// at most [`RETENTION`] in all; the `pools` and the runs `in_flight`.
/// `omitted` counts the records left out, by shape, age, number or size
/// ([`fit`]); a record's absence is no proof the pass did not run.
pub(super) fn document(
    at: &str,
    pass: Value,
    previous: Option<&Value>,
    (pools, in_flight): (Value, Value),
    now: OffsetDateTime,
) -> Value {
    let fresh = |record: &Value| {
        let finished = record["finished_at"].as_str();
        let finished = finished.and_then(|t| OffsetDateTime::parse(t, &Rfc3339).ok());
        shaped(record) && finished.is_some_and(|t| now - t <= time::Duration::days(RETENTION_DAYS))
    };
    let earlier: Vec<&Value> = (previous.filter(|p| p["schema"] == SCHEMA))
        .and_then(|p| p["passes"].as_array())
        .into_iter()
        .flatten()
        .filter(|p| p.is_object())
        .collect();
    let carried: Vec<Value> = (earlier.iter().filter(|p| fresh(p)))
        .take(RETENTION - 1)
        .map(|p| (*p).clone())
        .collect();
    let passes: Vec<Value> = std::iter::once(pass).chain(carried).collect();
    let mut document = json!({
        "schema": SCHEMA, "published_at": at, "retention": RETENTION,
        "omitted": earlier.len() + 1 - passes.len(),
        "passes": passes, "pools": pools, "in_flight": in_flight,
    });
    fit(&mut document);
    document
}

/// Cuts `document` down until its JSON is within [`MAX_BYTES`], the least
/// telling first: the oldest record's last no-start reason or start,
/// counted in its `omitted`; then, once no record has details, the oldest
/// whole record, counted in the document's; then the last run in flight.
pub(super) fn fit(document: &mut Value) {
    while document.to_string().len() > MAX_BYTES {
        if !cut(document) {
            return;
        }
    }
}

/// One cut of [`fit`]; whether anything was cut.
fn cut(document: &mut Value) -> bool {
    let bump = |counter: &mut Value| {
        *counter = (counter.as_u64().unwrap_or_default() + 1).into();
    };
    let Some(passes) = document["passes"].as_array_mut() else {
        return false;
    };
    for pass in passes.iter_mut().rev() {
        for list in ["no_start", "starts"] {
            if pass[list].as_array_mut().and_then(Vec::pop).is_some() {
                bump(&mut pass["omitted"][list]);
                return true;
            }
        }
    }
    let evicted = passes.len() > 1 && passes.pop().is_some();
    if evicted {
        bump(&mut document["omitted"]);
        return true;
    }
    if document["in_flight"]["runs"]
        .as_array_mut()
        .and_then(Vec::pop)
        .is_some()
    {
        bump(&mut document["in_flight"]["omitted"]);
        return true;
    }
    false
}

/// The comment: [`MARKER`], a line for a person, and `document` in a JSON
/// fence.
pub(super) fn body(document: &Value) -> String {
    let at = document["published_at"].as_str().unwrap_or_default();
    format!(
        "{MARKER}\nPass history as of {at}. Written by `agent-board pass`; the control app reads the JSON below.\n\n```json\n{document}\n```\n"
    )
}

/// The document of a comment [`body`] wrote, where it is one of
/// [`SCHEMA`].
pub(super) fn published(body: &str) -> Option<Value> {
    let rest = body.strip_prefix(MARKER)?;
    let (_, fenced) = rest.split_once("```json\n")?;
    let (json, _) = fenced.split_once("\n```")?;
    let document: Value = serde_json::from_str(json).ok()?;
    (document["schema"] == SCHEMA).then_some(document)
}

// ---- The pass ----

impl Bounds {
    /// Records a start the dispatch `inputs` ask for, in `state` with the
    /// run `answer` named; `kind` is the start's where the stage set one.
    pub(super) fn record_start(
        &self,
        state: StartState,
        (caller, inputs): (Option<&str>, &Value),
        kind: &str,
        run: Option<u64>,
    ) {
        let number = text(inputs, "/item").parse().unwrap_or_default();
        let item = (text(inputs, "/repo"), number);
        let run = caller.zip(run);
        let start = Start::new(state, item, kind, inputs["pool"].as_str(), run);
        self.starts.borrow_mut().extend(start);
    }

    /// Records a preview's start of `kind` on `item` in `pool`.
    pub(super) fn preview_start(&self, item: &ItemRef, kind: &str, pool: Option<&str>) {
        let on = (item.repository.as_str(), item.number);
        let start = Start::new(StartState::Preview, on, kind, pool, None);
        self.starts.borrow_mut().extend(start);
    }
}

impl Report {
    /// Ends the section under way and begins `name`'s ([`Sections`]);
    /// `refused` counts the requests the budget refused or the breaker did
    /// not send so far, and `cut` whether the ending stage's share refused
    /// a read.
    pub(super) fn enter_section(&mut self, name: Option<&str>, refused: usize, cut: bool) {
        let marks = Marks {
            failures: self.failures,
            operations: self.failed_operations.len(),
            deferred: self.deferred,
            refused,
        };
        self.sections
            .enter(name, marks, cut, &self.failed_operations);
    }
}

impl<C: Rest + Transport + Broker> Scheduler<'_, C> {
    /// Begins `stage`'s section ([`Report::enter_section`]), or with none
    /// ends the last.
    pub(super) fn section(&mut self, stage: Option<Stage>) {
        let bounds = self.client.bounds;
        let refused = bounds.refused.get() + bounds.not_sent.get();
        let cut = bounds.cut.get() > 0 || bounds.rate_limited.borrow().is_some();
        let name = stage.map(section_name);
        self.report.enter_section(name.as_deref(), refused, cut);
    }

    /// Begins `stage`: its section, and its requests, which leave `kept`
    /// to the stages after it ([`Bounds::enter_keeping`]).
    pub(super) fn enter(&mut self, stage: Stage, kept: usize) {
        self.section(Some(stage));
        self.client.bounds.enter_keeping(stage, kept);
    }

    /// This pass's [`record`].
    fn pass_record(&self, at: &str) -> Value {
        // The configured endpoints' hosts, without their ports: an error may
        // name the host alone.
        let hosts: Vec<String> = [
            self.config.usage.as_ref().and_then(|u| u.url.as_deref()),
            self.config.budget.as_ref().and_then(|b| b.url.as_deref()),
        ]
        .into_iter()
        .flatten()
        .filter_map(|url| {
            let authority = url.split_once("://")?.1;
            let authority = authority.split('/').next().unwrap_or(authority);
            let host = match authority.rsplit_once(':') {
                Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
                _ => authority,
            };
            Some(host.trim_matches(['[', ']']).to_owned())
        })
        .collect();
        let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
        let sections = self.report.sections.all();
        let starts = self.client.bounds.starts.borrow();
        // No ready work only where stage 4 ran whole: cut short or failed,
        // it may have left ready work unread.
        let schedule = section_name(Stage::Schedule);
        let whole = (sections.iter()).any(|s| s.name == schedule && s.outcome == Outcome::Ok);
        let idle =
            whole && self.free.is_some_and(|free| free > 0) && self.report.tally.1.started == 0;
        let no_start = no_starts(&self.report.holds, idle);
        let candidates = (self.report.tally.1.candidates).map(|n| {
            (
                n.saturating_sub(self.unread),
                self.unread,
                self.free.unwrap_or(0),
            )
        });
        record(&Pass {
            run: self
                .own
                .map(|own| (own.repository.as_str(), own.id, own.attempt)),
            at,
            mode: self.report.tally.1.mode,
            sections: &sections,
            starts: &starts,
            no_start: &no_start,
            candidates,
            hosts: &hosts,
        })
    }

    /// Publishes the pass history where `holds.passes` is set: the bot's
    /// comment with [`MARKER`] on the holds issue, found on its first page
    /// (the control app reads no further), carried over and this pass put
    /// first ([`document`]). One write step, after every stage, whatever
    /// they did, within the publication's share of requests and
    /// `--max-writes`; a read or write that fails is one line, and nothing
    /// is retried. Previews say what they would write, and write nothing;
    /// a `--pull` pass, which publishes nothing and is not serialized
    /// against the scheduled one, does not write it either.
    pub(super) fn publish_passes(&mut self) {
        self.section(None);
        let Some(at) = self.config.holds.as_ref().filter(|h| h.passes) else {
            return;
        };
        if self.config.pull.is_some() {
            return;
        }
        let on = (at.repository.as_str(), at.issue);
        let said = format!("passes: publish to {}#{}", at.repository, at.issue);
        let bounds = self.client.bounds;
        // Within the publication's share, and reusing the holds' reads.
        bounds.enter(Stage::Holds);
        if bounds.available() < WRITE_RESERVE + HOLDS_READS || Instant::now() > bounds.admit_by {
            let why = "the pass's request budget or time is spent";
            self.line(format!("passes: not published; {why}"));
            return;
        }
        let stamp = self.now.format(&Rfc3339).unwrap_or_default();
        let pass = self.pass_record(&stamp);
        let what = (&[MARKER][..], "pass history");
        let found = bot_comments(
            &self.client,
            &self.config.bot,
            on,
            what,
            (true, HOLDS_PAGES),
        );
        let (comment, seen) = match found {
            Ok((found, seen)) => (found.into_iter().next(), seen),
            Err(error) => {
                self.report.error_line(
                    "passes",
                    &error,
                    format!("passes error: {said}: {error:#}"),
                );
                return;
            }
        };
        if comment.is_none() && seen >= HOLDS_PAGES * COMMENTS_PER_PAGE {
            let why = "a new comment would fall past the first page the control app reads";
            self.line(format!("passes: not published; {why}"));
            return;
        }
        let previous = comment.as_ref().and_then(|c| published(text(c, "/body")));
        let configured: Vec<String> = (self.config.pools.0.iter())
            .map(|(n, _)| n.clone())
            .collect();
        let usage = self.snapshot.clone().flatten();
        let pools = pools(usage.as_ref(), &configured);
        let flights = self.flights.borrow();
        let in_flight = in_flight(&stamp, flights.as_deref());
        drop(flights);
        let document = document(
            &stamp,
            pass,
            previous.as_ref(),
            (pools, in_flight),
            self.now,
        );
        let body = body(&document);
        self.report.passes = Some(document);
        if let Some(held) = self.caps.check(Kind::Write) {
            self.line(format!("{held}: {said}"));
            return;
        }
        if !self.apply {
            self.caps.count(Kind::Write);
            self.line(format!("{said}; a preview does not write it"));
            return;
        }
        match self.publish(on, comment.as_ref(), "pass history", body) {
            Ok(url) => self.line(format!("passes: published {url}")),
            Err(error) => {
                self.report.error_line(
                    "passes",
                    &error,
                    format!("passes error: {said}: {error:#}"),
                );
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const AT: &str = "2026-11-05T12:00:00Z";

    /// The control app's parser (review `src/github/passes.ts`, its bounds
    /// in review `docs/passes.md`), as this document's contract: whether it
    /// takes `document` whole.
    pub(in crate::pass) fn accepted(document: &Value) -> bool {
        let time = |v: &Value| {
            v.as_str()
                .is_some_and(|t| OffsetDateTime::parse(t, &Rfc3339).is_ok())
        };
        let or_null = |v: &Value, check: &dyn Fn(&Value) -> bool| v.is_null() || check(v);
        let token = |v: &Value| v.as_str().is_some_and(|t| view::token(t).is_some());
        let one_of = |v: &Value, of: &[&str]| v.as_str().is_some_and(|s| of.contains(&s));
        let count = |v: &Value| v.as_u64().is_some_and(|n| n <= 1_000_000);
        let positive = |v: &Value| v.as_u64().is_some_and(|n| n > 0);
        let repository = |v: &Value| {
            v.as_str().is_some_and(|r| {
                r.split('/').count() == 2
                    && r.split('/').all(|s| {
                        !s.is_empty()
                            && s.len() <= 100
                            && s != "."
                            && s != ".."
                            && s.bytes()
                                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
                    })
            })
        };
        let item = |v: &Value| {
            v.as_str().is_some_and(|i| {
                i.split_once('#').is_some_and(|(r, n)| {
                    repository(&r.into())
                        && !n.starts_with('0')
                        && (1..=10).contains(&n.len())
                        && n.bytes().all(|c| c.is_ascii_digit())
                })
            })
        };
        let run = |v: &Value| v.is_object() && repository(&v["repository"]) && positive(&v["id"]);
        let list = |v: &Value, max: usize, check: &dyn Fn(&Value) -> bool| {
            v.as_array()
                .is_some_and(|l| l.len() <= max && l.iter().all(check))
        };
        let text = |v: &Value| {
            v.as_str().is_some_and(|t| {
                !t.is_empty() && t.chars().count() <= 200 && !t.chars().any(|c| c.is_control())
            })
        };
        let percent = |v: &Value| {
            v.as_f64()
                .is_some_and(|p| p.is_finite() && (0.0..=1000.0).contains(&p))
        };
        let section = |s: &Value| {
            token(&s["name"])
                && one_of(&s["outcome"], &["ok", "failed", "skipped", "cut-short"])
                && match s["outcome"] == "failed" {
                    true => text(&s["error"]),
                    false => s["error"].is_null(),
                }
        };
        let start = |s: &Value| {
            one_of(&s["state"], &["confirmed", "uncertain", "preview"])
                && item(&s["item"])
                && token(&s["kind"])
                && or_null(&s["pool"], &token)
                && or_null(&s["run"], &run)
                && (s["state"] != "preview" || s["run"].is_null())
        };
        let no_start = |r: &Value| {
            one_of(
                &r["reason"],
                &[
                    "no-ready-work",
                    "replan-wait",
                    "capped",
                    "held",
                    "newer-token",
                ],
            ) && or_null(&r["item"], &item)
                && or_null(&r["cap"], &token)
                && or_null(&r["hold"], &token)
                && match r["reason"] == "replan-wait" {
                    true => time(&r["until"]),
                    false => r["until"].is_null(),
                }
        };
        let pass = |p: &Value| {
            or_null(&p["run"], &|r| run(r) && positive(&r["attempt"]))
                && time(&p["started_at"])
                && time(&p["finished_at"])
                && one_of(&p["mode"], &["actions", "preview", "local"])
                && one_of(&p["result"], &["success", "partial", "failure"])
                && list(&p["sections"], 16, &section)
                && list(&p["starts"], 20, &start)
                && list(&p["no_start"], 20, &no_start)
                && (p["mode"] != "actions"
                    || !p["starts"]
                        .as_array()
                        .is_some_and(|s| s.iter().any(|s| s["state"] == "preview")))
                && or_null(&p["candidates"], &|c| {
                    count(&c["read"]) && count(&c["unread"]) && count(&c["ready"])
                })
                && ["sections", "starts", "no_start"]
                    .iter()
                    .all(|k| count(&p["omitted"][k]))
        };
        let pool = |p: &Value| {
            token(&p["pool"])
                && or_null(&p["observed_at"], &time)
                && or_null(&p["used_percent"], &percent)
                && or_null(&p["allowed_percent"], &percent)
                && or_null(&p["resets_at"], &time)
                && (p["held"].is_null() || p["held"].is_boolean())
                && or_null(&p["ready"], &count)
        };
        let pools = |p: &Value| {
            or_null(&p["observed_at"], &time)
                && p["complete"].is_boolean()
                && count(&p["omitted"])
                && list(&p["items"], 16, &pool)
        };
        let flight = |f: &Value| {
            item(&f["item"])
                && token(&f["kind"])
                && or_null(&f["pool"], &token)
                && run(&f["run"])
                && or_null(&f["since"], &time)
        };
        let pr = |p: &Value| {
            repository(&p["repository"])
                && positive(&p["number"])
                && one_of(&p["state"], &["merge-queue", "review-wait"])
                && or_null(&p["since"], &time)
        };
        let in_flight = |f: &Value| {
            time(&f["observed_at"])
                && f["complete"].is_boolean()
                && count(&f["omitted"])
                && list(&f["runs"], 50, &flight)
                && list(&f["prs"], 50, &pr)
        };
        document["schema"] == SCHEMA
            && time(&document["published_at"])
            && document["retention"] == RETENTION
            && count(&document["omitted"])
            && list(&document["passes"], RETENTION, &pass)
            && or_null(&document["pools"], &pools)
            && or_null(&document["in_flight"], &in_flight)
            && body(document).chars().count() <= holds::MAX_COMMENT_CHARS
    }

    /// The sections of a pass whose first `ran` stages ran, `outcomes` for
    /// the first of them, in order, the rest ok.
    fn sections(outcomes: &[(Outcome, Option<&str>)], ran: usize) -> Vec<Section> {
        let mut sections = Sections::default();
        let marks = |failures, operations, deferred| Marks {
            failures,
            operations,
            deferred,
            refused: 0,
        };
        let mut errors: Vec<String> = Vec::new();
        let (mut failures, mut deferred) = (0, 0);
        for (n, stage) in Stage::ALL.iter().take(ran).enumerate() {
            let name = section_name(*stage);
            sections.enter(
                Some(name.as_str()),
                marks(failures, errors.len(), deferred),
                false,
                &errors,
            );
            match outcomes.get(n) {
                Some((Outcome::Failed, error)) => {
                    failures += 1;
                    errors.push(error.unwrap_or("it failed").to_owned());
                }
                Some((Outcome::CutShort, _)) => deferred += 1,
                _ => {}
            }
        }
        sections.enter(
            None,
            marks(failures, errors.len(), deferred),
            false,
            &errors,
        );
        sections.all()
    }

    fn start(state: StartState, n: u64, kind: &str, run: Option<u64>) -> Start {
        let run = run.map(|id| ("o/runner", id));
        Start::new(state, ("o/work", n), kind, Some("claude"), run).unwrap()
    }

    fn usage() -> Value {
        json!({"schema": USAGE_SCHEMA, "updated_at": "2026-11-05T11:58:00Z", "pools": {
            "claude": {"observed_at": "2026-11-05T11:57:30Z", "used_percent": 45.44, "allowed_percent": 50.0,
                "resets_at": "2026-11-05T13:00:00Z", "hold": false},
            "openai": {"reason": "no-reading", "hold": true},
            "Bad Name": {"used_percent": 1.0}}})
    }

    fn flights() -> Vec<view::Run> {
        let since = Some("2026-11-05T11:00:00Z");
        let run = |n| view::Run::new(("o/work", n), "fix", None, ("o/runner", 100 + n), since);
        (1..=2).filter_map(run).collect()
    }

    fn holds() -> Vec<Entry> {
        let entry = |kind, scope| Entry {
            scope,
            hold: Hold::new(kind, "$1003 of $1000 spent at https://broker.ts.net:8443/x"),
            priority: None,
            why: None,
        };
        let item = |n| {
            Scope::Item(ItemRef {
                host: DEFAULT_HOST.into(),
                repository: "o/work".into(),
                number: n,
            })
        };
        vec![
            entry(HoldKind::NeedsHuman, item(3)),
            entry(HoldKind::Budget, Scope::Pass),
            entry(HoldKind::Blocked, item(9)),
        ]
    }

    /// The fixtures in `fixtures/passes/`, as the producer emits them: a
    /// pass whose setup failed before stage 4, one whose stage 2 failed and
    /// stage 4 was cut short with an accepted dispatch GitHub did not name
    /// and one whose answer was lost, and a preview whose starts were never
    /// sent; each carrying an earlier record and the pools and runs read.
    fn emitted(name: &str) -> Value {
        let hosts = ["broker.example"];
        let ok = (Outcome::Ok, None);
        let earlier = json!({
            "run": {"repository": "o/runner", "id": 41, "attempt": 1},
            "started_at": "2026-11-05T11:30:00Z", "finished_at": "2026-11-05T11:30:00Z",
            "mode": "actions", "result": "success",
            "sections": [], "starts": [], "no_start": [], "candidates": null,
            "omitted": {"sections": 0, "starts": 0, "no_start": 0},
        });
        let stale = json!({"finished_at": "2026-10-01T00:00:00Z", "mode": "actions"});
        let previous = json!({"schema": SCHEMA, "passes": [earlier, stale]});
        let (sections, starts, no_start, run, mode, candidates) = match name {
            "failed" => (
                sections(
                    &[(
                        Outcome::Failed,
                        Some(
                            "dispatch ref error: reading o/caller: HTTP 503 from https://broker.example/v1 at 10.0.0.7:8443",
                        ),
                    )],
                    1,
                ),
                vec![],
                vec![],
                Some(("o/runner", 42, 2)),
                "actions",
                None,
            ),
            "partial" => (
                sections(
                    &[
                        ok,
                        ok,
                        ok,
                        ok,
                        (
                            Outcome::Failed,
                            Some("follow error: API rate limit exceeded for installation"),
                        ),
                        ok,
                        (Outcome::CutShort, None),
                    ],
                    Stage::ALL.len(),
                ),
                vec![
                    start(StartState::Confirmed, 1, "implement", Some(1001)),
                    start(StartState::Confirmed, 2, "triage", None),
                    start(StartState::Uncertain, 4, "fix", None),
                ],
                no_starts(&holds(), false),
                Some(("o/runner", 43, 1)),
                "actions",
                Some((6, 9, 0)),
            ),
            _ => (
                sections(&[], Stage::ALL.len()),
                vec![start(StartState::Preview, 5, "research", None)],
                no_starts(&holds()[..1], true),
                None,
                "preview",
                Some((15, 0, 1)),
            ),
        };
        let pass = record(&Pass {
            run,
            at: AT,
            mode,
            sections: &sections,
            starts: &starts,
            no_start: &no_start,
            candidates,
            hosts: &hosts,
        });
        let configured = ["claude".to_owned(), "openai".to_owned()];
        let pools = pools(Some(&usage()), &configured);
        let now = OffsetDateTime::parse(AT, &Rfc3339).unwrap();
        let flights = flights();
        let in_flight = in_flight(AT, Some(&flights[..]));
        document(AT, pass, Some(&previous), (pools, in_flight), now)
    }

    #[test]
    fn emitted_documents_match_the_fixtures_and_are_accepted() {
        for name in ["failed", "partial", "preview"] {
            let document = emitted(name);
            assert!(accepted(&document), "{name}: {document:#}");
            let path = format!(
                "{}/../../fixtures/passes/{name}.json",
                env!("CARGO_MANIFEST_DIR")
            );
            let fixture: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(document, fixture, "{name}: {document:#}");
            // The published comment reads back as the document.
            assert_eq!(
                published(&body(&document)),
                Some(document.clone()),
                "{name}"
            );
        }
        let failed = emitted("failed");
        assert_eq!(failed["passes"][0]["result"], "failure");
        assert_eq!(
            failed["passes"][0]["sections"][0]["error"],
            "dispatch ref error: reading o/caller: HTTP 503 from <address> at <address>"
        );
        let outcomes: Vec<_> = (failed["passes"][0]["sections"].as_array().unwrap().iter())
            .map(|s| s["outcome"].as_str().unwrap())
            .collect();
        assert_eq!(outcomes[0], "failed");
        assert!(
            outcomes[1..].iter().all(|o| *o == "skipped"),
            "{outcomes:?}"
        );
        // The stale record went by age, and the earlier one is carried.
        assert_eq!(failed["omitted"], 1);
        assert_eq!(failed["passes"][1]["run"]["id"], 41);
        let partial = emitted("partial");
        assert_eq!(partial["passes"][0]["result"], "partial");
        let states: Vec<_> = (partial["passes"][0]["starts"].as_array().unwrap().iter())
            .map(|s| (s["state"].clone(), s["run"]["id"].clone()))
            .collect();
        assert_eq!(
            states,
            [
                (json!("confirmed"), json!(1001)),
                (json!("confirmed"), Value::Null),
                (json!("uncertain"), Value::Null)
            ]
        );
        assert!(!partial.to_string().contains('$'), "{partial}");
        let preview = emitted("preview");
        assert_eq!(preview["passes"][0]["result"], "success");
        assert_eq!(
            preview["passes"][0]["no_start"][0]["reason"],
            "no-ready-work"
        );
    }

    #[test]
    fn checker_takes_the_fixture_and_refuses_what_the_reader_refuses() {
        let document = emitted("partial");
        let cases: &[(&str, Value)] = &[
            ("/schema", json!("agent-board/passes/v2")),
            ("/retention", json!(11)),
            ("/passes/0/sections/0/outcome", json!("done")),
            ("/passes/0/sections/0/error", json!("not failed")),
            ("/passes/0/starts/0/state", json!("sent")),
            ("/passes/0/starts/0/kind", json!("Implement")),
            ("/passes/0/no_start/0/reason", json!("busy")),
            ("/passes/0/run/attempt", json!(0)),
            ("/pools/items/0/used_percent", json!(1001)),
            ("/in_flight/runs/0/run/id", json!(0)),
        ];
        for (at, value) in cases {
            let mut document = document.clone();
            *document.pointer_mut(at).unwrap() = value.clone();
            assert!(!accepted(&document), "{at}");
        }
    }

    #[test]
    fn sections_are_judged_by_the_counts_while_they_ran() {
        let all = sections(
            &[
                (Outcome::Ok, None),
                (Outcome::Failed, Some("boom")),
                (Outcome::CutShort, None),
            ],
            Stage::ALL.len(),
        );
        let judged: Vec<_> = all.iter().map(|s| (s.name.as_str(), s.outcome)).collect();
        assert_eq!(
            judged[..4],
            [
                ("setup", Outcome::Ok),
                ("changes", Outcome::Failed),
                ("usage", Outcome::CutShort),
                ("maintain", Outcome::Ok)
            ]
        );
        assert_eq!(all[1].error.as_deref(), Some("boom"));
        assert_eq!(judged[4].0, "pull-requests");
        // A pass that stopped at setup skips the rest; a stage under way is
        // not judged.
        let mut stopped = Sections::default();
        stopped.enter(Some("setup"), Marks::default(), false, &[]);
        let marks = Marks {
            failures: 1,
            operations: 0,
            ..Marks::default()
        };
        stopped.enter(None, marks, false, &["dispatch ref error: x".into()]);
        let all = stopped.all();
        assert_eq!(
            (all[0].outcome, all[0].error.as_deref()),
            (Outcome::Failed, Some("dispatch ref error: x"))
        );
        assert!(all[1..].iter().all(|s| s.outcome == Outcome::Skipped));
        let mut open = Sections::default();
        open.enter(Some("setup"), Marks::default(), false, &[]);
        assert!(open.all().iter().all(|s| s.outcome == Outcome::Skipped));
        // Refused reads and a tripped breaker cut a stage short.
        let mut refused = Sections::default();
        refused.enter(Some("schedule"), Marks::default(), false, &[]);
        let marks = Marks {
            refused: 2,
            ..Marks::default()
        };
        refused.enter(None, marks, false, &[]);
        assert_eq!(refused.done[0].outcome, Outcome::CutShort);
    }

    /// GitHub's answer decides a start's state: accepted, refused (a 4xx,
    /// or an answer the client gave up on), or uncertain.
    #[test]
    fn start_state_follows_the_answer() {
        let refused = anyhow::anyhow!("POST x: GitHub HTTP status 429: slow down")
            .context(RateLimited("secondary limit".into()));
        let unauthenticated = anyhow::Error::from(crate::github::AuthenticationRequired)
            .context("POST x: GitHub HTTP status 401");
        type Case = (Result<(u16, Value)>, Option<StartState>);
        let cases: [Case; 6] = [
            (Ok((200, json!({}))), Some(StartState::Confirmed)),
            (Ok((204, Value::Null)), Some(StartState::Confirmed)),
            (Ok((422, json!({}))), None),
            (Ok((502, json!({}))), Some(StartState::Uncertain)),
            (
                Err(anyhow::anyhow!("connection reset")),
                Some(StartState::Uncertain),
            ),
            (Err(refused), None),
        ];
        for (answer, want) in &cases {
            assert_eq!(start_state(answer), *want, "{answer:?}");
        }
        assert_eq!(start_state(&Err(unauthenticated)), None);
    }

    /// A start the reader would refuse is left out; a run of id 0 or in a
    /// repository it takes for a path is none.
    #[test]
    fn starts_are_only_those_the_reader_takes() {
        let new = |repo: &str, n, kind: &str, run| {
            Start::new(
                StartState::Confirmed,
                (repo, n),
                kind,
                Some("Big Pool"),
                run,
            )
        };
        assert_eq!(new("o/..", 1, "fix", None), None);
        assert_eq!(new("o/r", 0, "fix", None), None);
        assert_eq!(new("o/r", 1, "Fix", None), None);
        let unnamed = new("o/r", 1, "review rerun", Some(("o/runner", 0))).unwrap();
        assert_eq!(
            unnamed.json(),
            json!({"state": "confirmed", "item": "o/r#1", "kind": "review-rerun", "pool": null, "run": null})
        );
        let named = new("o/r", 1, "fix", Some(("o/runner", 7))).unwrap();
        assert_eq!(
            named.json()["run"],
            json!({"repository": "o/runner", "id": 7})
        );
    }

    #[test]
    fn redaction_takes_out_addresses_and_bounds_the_line() {
        let cases = [
            ("plain text", "plain text"),
            (
                "GET https://broker.example/v1/usage: HTTP 503",
                "GET <address>: HTTP 503",
            ),
            ("dial 10.1.2.3:443 failed", "dial <address> failed"),
            ("dial 10.1.2.3 failed", "dial <address> failed"),
            ("host inference.ts.net refused", "host <address> refused"),
            ("at broker.example:8080, then", "at <address>, then"),
            ("two\nlines", "two"),
            ("tab\there", "tabhere"),
            ("", "error"),
            ("v1.2.3 of pass.rs", "v1.2.3 of pass.rs"),
            ("at 12:30 today", "at <address> today"),
            ("dial fd7a:115c:a1e0::1 failed", "dial <address> failed"),
            (
                "dial [fd7a:115c:a1e0::1]:8443: refused",
                "dial <address>: refused",
            ),
            (
                "time 2026-11-05T12:00:00Z passed",
                "time 2026-11-05T12:00:00Z passed",
            ),
        ];
        for (text, want) in cases {
            assert_eq!(redact(text, &[]), want, "{text:?}");
        }
        // A configured host wherever it appears, whatever its case.
        assert_eq!(
            redact(
                "the Broker.example answered, then host=broker.example/v1 did not",
                &["broker.example"]
            ),
            "the <address> answered, then host=<address>/v1 did not"
        );
        let long = redact(&"x".repeat(300), &[]);
        assert_eq!(long.chars().count(), MAX_ERROR_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn no_starts_name_caps_and_holds_without_reasons() {
        let reasons: Vec<Value> = no_starts(&holds(), true)
            .iter()
            .map(NoStart::json)
            .collect();
        assert_eq!(
            Value::Array(reasons),
            json!([
                {"reason": "no-ready-work", "item": null, "until": null, "cap": null, "hold": null},
                {"reason": "capped", "item": null, "until": null, "cap": "budget", "hold": null},
                {"reason": "held", "item": "o/work#3", "until": null, "cap": null, "hold": "needs-human"},
                {"reason": "held", "item": "o/work#9", "until": null, "cap": null, "hold": "blocked"}])
        );
    }

    #[test]
    fn pools_keep_each_readings_clock_and_in_flight_its_runs() {
        let configured = [
            "claude".to_owned(),
            "openai".to_owned(),
            "mistral".to_owned(),
        ];
        let pools = pools(Some(&usage()), &configured);
        assert_eq!(
            pools,
            json!({"observed_at": "2026-11-05T11:58:00Z", "complete": false, "omitted": 1, "items": [
                {"pool": "claude", "observed_at": "2026-11-05T11:57:30Z", "used_percent": 45.4, "allowed_percent": 50.0,
                    "resets_at": "2026-11-05T13:00:00Z", "held": false, "ready": null},
                {"pool": "openai", "observed_at": null, "used_percent": null, "allowed_percent": null,
                    "resets_at": null, "held": true, "ready": null}]})
        );
        assert_eq!(super::pools(None, &configured), Value::Null);
        let flights = flights();
        let in_flight = in_flight(AT, Some(&flights[..]));
        assert_eq!(in_flight["runs"][0]["since"], "2026-11-05T11:00:00Z");
        assert_eq!(in_flight["runs"][0].get("started_at"), None);
        assert_eq!(
            (&in_flight["complete"], &in_flight["omitted"]),
            (&json!(true), &json!(0))
        );
        assert_eq!(super::in_flight(AT, None), Value::Null);
    }

    /// Past the size bound the oldest details go first, counted in their
    /// record, then whole records, counted in the document; the newest
    /// record alone always stays. Past the retention the oldest are left
    /// out, and a document of another schema carries nothing.
    #[test]
    fn documents_are_bounded_by_retention_and_size() {
        let now = OffsetDateTime::parse(AT, &Rfc3339).unwrap();
        let many: Vec<Start> = (1..=MAX_STARTS as u64 + 5)
            .map(|n| start(StartState::Confirmed, n, "implement", Some(1000 + n)))
            .collect();
        let pass = |sections: &[Section]| {
            record(&Pass {
                run: Some(("o/runner", 42, 1)),
                at: AT,
                mode: "actions",
                sections,
                starts: &many,
                no_start: &no_starts(&holds(), false),
                candidates: Some((3, 0, 0)),
                hosts: &[],
            })
        };
        let full = pass(&sections(&[], Stage::ALL.len()));
        assert_eq!(full["omitted"]["starts"], 5);
        assert_eq!(full["starts"].as_array().unwrap().len(), MAX_STARTS);
        let mut previous = None;
        for n in 0..RETENTION + 2 {
            let document = document(
                AT,
                full.clone(),
                previous.as_ref(),
                (Value::Null, Value::Null),
                now,
            );
            assert!(accepted(&document), "{document}");
            assert_eq!(
                document["passes"].as_array().unwrap().len(),
                (n + 1).min(RETENTION)
            );
            // Counted for this publication alone: the one record past the
            // retention, not every record ever evicted.
            assert_eq!(document["omitted"], json!(usize::from(n >= RETENTION)));
            previous = Some(document);
        }
        let other = json!({"schema": "agent-board/pass-view/v1", "passes": [full.clone()]});
        let alone = document(
            AT,
            full.clone(),
            Some(&other),
            (Value::Null, Value::Null),
            now,
        );
        assert_eq!(alone["passes"].as_array().unwrap().len(), 1);
        // A record without a record's shape is left out, counted.
        let mut unshaped = full.clone();
        unshaped["starts"] = Value::Null;
        let edited =
            json!({"schema": SCHEMA, "passes": [unshaped, {"finished_at": AT}, full.clone()]});
        let kept = document(
            AT,
            full.clone(),
            Some(&edited),
            (Value::Null, Value::Null),
            now,
        );
        assert_eq!(
            (kept["passes"].as_array().unwrap().len(), &kept["omitted"]),
            (2, &json!(2))
        );
        // A document past the size: a long error in every section of every record.
        let long: Vec<Section> = sections(&[], Stage::ALL.len())
            .into_iter()
            .map(|s| Section {
                outcome: Outcome::Failed,
                error: Some("e".repeat(MAX_ERROR_CHARS)),
                ..s
            })
            .collect();
        let heavy = pass(&long);
        let flights = flights();
        let mut previous = None;
        for _ in 0..RETENTION {
            let listed = in_flight(AT, Some(&flights[..]));
            let document = document(
                AT,
                heavy.clone(),
                previous.as_ref(),
                (Value::Null, listed),
                now,
            );
            assert!(
                document.to_string().len() <= MAX_BYTES,
                "{}",
                document.to_string().len()
            );
            assert!(accepted(&document), "{document}");
            previous = Some(document);
        }
        let document = previous.unwrap();
        let passes = document["passes"].as_array().unwrap();
        // Every record stays while one has details to give: the newest kept
        // every start, the oldest lost its details first, counted.
        assert_eq!((passes.len(), &document["omitted"]), (RETENTION, &json!(0)));
        assert_eq!(passes[0]["starts"].as_array().unwrap().len(), MAX_STARTS);
        let last = passes.last().unwrap();
        assert!(last["omitted"]["starts"].as_u64().unwrap() > 5, "{last}");
        // Without details, the oldest whole record goes, then the last run in
        // flight; one record without details stays whatever the size.
        let record = json!({"starts": [], "no_start": [], "omitted": {"starts": 0, "no_start": 0}});
        let mut bare = json!({"omitted": 0, "passes": [record, record],
            "in_flight": {"omitted": 0, "runs": [{"item": "o/r#1"}]}});
        assert!(cut(&mut bare));
        assert_eq!(
            (bare["passes"].as_array().unwrap().len(), &bare["omitted"]),
            (1, &json!(1))
        );
        assert!(cut(&mut bare));
        assert_eq!(
            (&bare["in_flight"]["runs"], &bare["in_flight"]["omitted"]),
            (&json!([]), &json!(1))
        );
        assert!(!cut(&mut bare));
    }
}
