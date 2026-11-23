//! Detect the PR metadata loop without giving review prose write authority.

use super::*;

const MAX_REVIEW_BYTES: usize = 24_000;
pub(super) const WAIT_PREFIX: &str = "recurring PR metadata blocker across ";

/// The verdict line that declares its sole blocker to be PR text the diff does
/// not deliver, followed by that text verbatim (workflow/review.md).
pub(super) const TEXT_BLOCKER: &str = "PR text blocker: ";

/// Bounds of the quoted phrase, so a word or two can't match any PR.
const PHRASE_BYTES: std::ops::RangeInclusive<usize> = 8..=500;

/// The phrase of `pull`'s title or body that a verdict declares its sole
/// blocker ([`TEXT_BLOCKER`]). None where the verdict has no such line, more
/// than one, a phrase out of bounds or one the PR text no longer contains:
/// review prose alone never pauses a fix, and editing the phrase out clears it.
fn text_blocker<'a>(pull: &Value, body: &'a str) -> Option<&'a str> {
    if body.len() > MAX_REVIEW_BYTES {
        return None;
    }
    let mut fields = body
        .lines()
        .filter_map(|line| line.strip_prefix(TEXT_BLOCKER));
    let (Some(phrase), None) = (fields.next(), fields.next()) else {
        return None;
    };
    let phrase = phrase.trim();
    (PHRASE_BYTES.contains(&phrase.len())
        && [text(pull, "/title"), text(pull, "/body")]
            .iter()
            .any(|said| said.contains(phrase)))
    .then_some(phrase)
}

/// Whether `at` is after the newest change of `pull`'s base, if there was one;
/// an unreadable time is not.
fn after_base(pull: &Value, at: Option<&str>) -> bool {
    nodes(pull, "/baseChange/nodes")
        .last()
        .is_none_or(|change| {
            timestamp(change["createdAt"].as_str())
                .is_some_and(|changed| timestamp(at).is_some_and(|at| at > changed))
        })
}

fn operator(config: &Config, review: &Value) -> bool {
    text(review, "/author/__typename") == "User" && text(review, "/author/login") == config.operator
}

fn review_app(review: &Value) -> bool {
    text(review, "/author/__typename") == "Bot"
        && review.pointer("/author/databaseId").and_then(Value::as_u64) == Some(ACTIONS_BOT_ID)
}

/// The immediately preceding possible verdict must be the change request that
/// the last round worked on, naming the same `phrase`. Repeat change requests
/// naming it on the current head aren't additional rounds. A different
/// intervening verdict or ambiguous ordering must not look like another
/// failed round.
fn predecessor<'a>(
    pull: &'a Value,
    config: &Config,
    current: &Value,
    old: &str,
    phrase: &str,
) -> Option<&'a Value> {
    let at = timestamp(current["submittedAt"].as_str())?;
    if pull.pointer("/reviews/pageInfo/hasPreviousPage") != Some(&Value::Bool(false))
        || nodes(pull, "/reviews/nodes").len() > 100
    {
        return None;
    }
    let mut prior = None;
    for review in nodes(pull, "/reviews/nodes") {
        if std::ptr::eq(review, current)
            || !(operator(config, review) || review_app(review))
            || !(matches!(
                text(review, "/state"),
                "APPROVED" | "CHANGES_REQUESTED" | "DISMISSED"
            ) || text(review, "/body").lines().next() == Some(WITHHELD))
        {
            continue;
        }
        let submitted = timestamp(review["submittedAt"].as_str())?;
        if submitted >= at {
            return None;
        }
        if !after_base(pull, review["submittedAt"].as_str()) {
            continue;
        }
        if review["commit"] == current["commit"]
            && review["state"] == "CHANGES_REQUESTED"
            && text_blocker(pull, text(review, "/body")) == Some(phrase)
        {
            continue;
        }
        match prior {
            Some((t, _)) if t == submitted => return None,
            Some((t, _)) if t > submitted => (),
            _ => prior = Some((submitted, review)),
        }
    }
    let (_, review) = prior?;
    (text(review, "/commit/oid") == old
        && review["state"] == "CHANGES_REQUESTED"
        && text_blocker(pull, text(review, "/body")) == Some(phrase))
    .then_some(review)
}

impl<R: Rest, G: Transport> Pass<'_, R, G> {
    pub(super) fn recurring_blocker(
        &mut self,
        repo: &str,
        pull: &Value,
        marker: &Marker,
        review_run: Option<&Value>,
    ) -> Option<String> {
        // Public bot-fork PRs only. Private fork workflows can impersonate the
        // Actions review app; the regular head-verdict gate doesn't trust them.
        if !self.config.ours(repo)
            || !from_fork(pull, &self.config.bot)
            || pull["baseRepository"]["isPrivate"] != false
            || marker.stopped
            || marker.rounds == 0
            || marker.rebasing
            || marker.answering
            || marker.continuing
            || marker.run.is_none()
        {
            return None;
        }
        let old = marker.sha.as_deref().filter(|old| !old.is_empty())?;
        let head = text(pull, "/headRefOid");
        let (Review::ChangesRequested, current) = head_verdict(pull, self.config, review_run)?
        else {
            return None;
        };
        // Both verdicts' review jobs must have started on the current base, as
        // for an approval ([`reviewed_on_base`]): one started before a
        // retarget reviewed against the old base.
        if old == head || !reviewed_on_base(pull, self.config, review_run) {
            return None;
        }
        let phrase = text_blocker(pull, text(current, "/body"))?;
        let prior = predecessor(pull, self.config, current, old, phrase)?;
        let started = timestamp(marker.at.as_deref())?;
        if timestamp(prior["submittedAt"].as_str())? > started
            || started >= timestamp(current["submittedAt"].as_str())?
        {
            return None;
        }
        if !operator(self.config, prior) {
            if nodes(pull, "/reviews/nodes").iter().any(|review| {
                text(review, "/commit/oid") == old
                    && operator(self.config, review)
                    && matches!(text(review, "/state"), "APPROVED" | "CHANGES_REQUESTED")
                    && timestamp(review["submittedAt"].as_str())
                        .is_some_and(|submitted| submitted <= started)
                    && after_base(pull, review["submittedAt"].as_str())
            }) {
                // Don't infer a fix from an app verdict the operator could
                // have overridden on the previous head.
                return None;
            }
            // One additional provenance read. A copied marker from a review
            // of a different head or outside that run's attempt cannot pause fixes.
            let (base, run) = cited_run(text(prior, "/body"))?;
            if base != repo
                || self.config.review_workflow.is_none()
                || self.remaining <= PULL_SHARE_MARGIN
                || self
                    .rest
                    .share_left()
                    .is_some_and(|left| left <= PULL_SHARE_MARGIN)
            {
                return None;
            }
            let previous = json!({
                "headRefOid": old, "number": pull["number"], "baseRepository": pull["baseRepository"]
            });
            let (run, _) = self
                .get_url(
                    &format!("{API_ROOT}repos/{repo}/actions/runs/{run}"),
                    slim_run,
                    false,
                )
                .ok()?;
            if !review_run_matches(&previous, prior, Some(&run), self.config)
                || !after_base(pull, run["run_started_at"].as_str())
            {
                return None;
            }
        }
        Some(format!(
            "{WAIT_PREFIX}{} and {}; pause automatic fixes until the PR text drops the quoted phrase or a new verdict on this head",
            short(old),
            short(head)
        ))
    }
}
