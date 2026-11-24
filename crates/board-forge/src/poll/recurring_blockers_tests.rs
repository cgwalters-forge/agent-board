#[test]
fn recurring_metadata_blockers_pause_fix_dispatch() {
    const OLD: &str = "c14ea602515931fbb9fa210dc1b9a9d9f87fa781";
    const OLDER_RUN: u64 = 44864557527;
    const PHRASE: &str = "Closes #3381";
    const BODY: &str = "Add a no-push readiness transition.\n\nCloses #3381";
    // Condensed from agentic-job #4399, with the field the review declares.
    use recurring_blockers::TEXT_BLOCKER;
    let field = format!("{TEXT_BLOCKER}{PHRASE}");
    let previous = format!(
        "- **blocker** — The PR title/body advertise a no-push readiness transition and `Closes #3381`, but this head leaves checked evidence diagnostic-only. Retitle and describe this as fail-closed regression coverage.\n\n{field}"
    );
    let pattern = format!(
        "- **blocker — Do not close #3381 with fail-closed coverage.** This contradicts the title/body. Retitle and describe this as regression coverage.\n\n{field}"
    );
    // Prose asking for a retitle, without the field, never matches.
    let prose = "- **blocker** — The title and body promise validation, but invalid records are accepted. Retitle and describe this as logging-only if validation is being deferred; otherwise reject invalid records.";
    for (case, expected) in [
        ("4399", "wait"),
        ("prose only", "fix"),
        ("prose prior", "fix"),
        ("different phrase", "fix"),
        ("phrase edited out", "fix"),
        ("short phrase", "fix"),
        ("two fields", "fix"),
        ("merge queue enabled", "wait"),
        ("default off", "fix"),
        ("visitor", "fix"),
        ("copied receipt", "fix"),
        ("same head", "fix"),
        ("intervening approval", "fix"),
        ("round before prior verdict", "fix"),
        ("repeat current head", "wait"),
        ("operator approved prior head", "fix"),
        ("retarget before both jobs", "wait"),
        ("prior job started before retarget", "fix"),
        ("red CI", "fix"),
        ("conflict", "fix"),
        ("placeholder", "fix"),
    ] {
        let mut old = review(
            "CHANGES_REQUESTED",
            if case == "visitor" {
                user("visitor")
            } else {
                job()
            },
            if case == "same head" { HEAD } else { OLD },
            match case {
                "prose prior" => prose.into(),
                "different phrase" => previous.replace(PHRASE, "no-push readiness transition"),
                _ => previous.clone(),
            }
            .as_str(),
        );
        old["body"] = text(&old, "/body")
            .replace(&REVIEW_RUN.to_string(), &OLDER_RUN.to_string())
            .into();
        let current = match case {
            "prose only" => prose.into(),
            "short phrase" => pattern.replace(PHRASE, "#3381"),
            "two fields" => format!("{pattern}\n{TEXT_BLOCKER}no-push readiness transition"),
            _ => format!("{pattern}\n\n- **nit** — Update the line reference."),
        };
        let mut reviews = vec![
            later(review("CHANGES_REQUESTED", job(), HEAD, &current)),
            old,
        ];
        if case == "repeat current head" {
            let mut repeat = review("CHANGES_REQUESTED", job(), HEAD, &pattern);
            repeat["submittedAt"] = "2026-10-12T01:15:00Z".into();
            reviews.push(repeat);
        } else if case == "intervening approval" {
            let mut approval = review("APPROVED", user("cgwalters"), "an-intervening-head", "");
            approval["submittedAt"] = "2026-10-12T01:15:00Z".into();
            reviews.push(approval);
        } else if case == "operator approved prior head" {
            let mut approval = review("APPROVED", user("cgwalters"), OLD, "");
            approval["submittedAt"] = "2026-10-12T00:58:00Z".into();
            reviews.push(approval);
        }
        let mut fake = github();
        let mut run = job_run(&json!(if case == "copied receipt" { HEAD } else { OLD }));
        run["id"] = OLDER_RUN.into();
        if case == "prior job started before retarget" {
            run["run_started_at"] = "2026-10-12T00:40:00Z".into();
        }
        fake.page(
            &format!("repos/{REPO}/actions/runs/{OLDER_RUN}"),
            None,
            None,
            run,
        );
        let mut change = json!({
            "reviews": {"nodes": reviews, "pageInfo": {"hasPreviousPage": false}},
            "body": if case == "phrase edited out" { "Add regression coverage." } else { BODY },
        });
        if matches!(
            case,
            "retarget before both jobs" | "prior job started before retarget"
        ) {
            change["baseChange"] = json!({"nodes": [{"createdAt": "2026-10-12T00:45:00Z"}]});
        }
        if case == "red CI" {
            change["contexts"] = json!({"ci": {"conclusion": "FAILURE"}});
        }
        if case == "conflict" {
            change["mergeable"] = json!(CONFLICTING);
        }
        if case == "placeholder" {
            change["series"] = json!({"nodes":[{"commit":{"messageHeadline":format!("{PLACEHOLDER_PREFIX}11")}}],"pageInfo":{"hasNextPage":false}});
        }
        let mut policy = PollPolicy::from_json(&if case == "default off" {
            json!({})
        } else {
            json!({"holdRecurringBlockers": true})
        })
        .unwrap();
        if case == "merge queue enabled" {
            policy.merge_queue = Some(3);
        }
        let started = if case == "round before prior verdict" {
            "2026-10-12T00:55:00Z"
        } else {
            "2026-10-12T01:10:00Z"
        };
        let (out, fake, dispatcher) = applied_policy(
            fake,
            policy,
            change,
            json!({"sha": OLD, "rounds": 1, "run": 11, "at": started}),
        );
        assert_eq!(out.errors, 0, "{case}: {:?}", out.lines);
        assert_eq!(out.steps[0].action, expected, "{case}: {:?}", out.lines);
        assert_eq!(out.dispatched, usize::from(expected == "fix"), "{case}");
        if expected == "wait" {
            assert!(
                out.steps[0]
                    .reason
                    .contains("recurring PR metadata blocker"),
                "{case}"
            );
            assert_eq!(out.waits[0].kind, "needs-human", "{case}");
            assert!(
                fake.writes.borrow().is_empty() && dispatcher.writes.borrow().is_empty(),
                "{case}"
            );
        }
        if case == "default off" {
            assert!(
                fake.sent
                    .borrow()
                    .iter()
                    .all(|(url, _)| !url.ends_with(&OLDER_RUN.to_string()))
            );
        }
    }
}
