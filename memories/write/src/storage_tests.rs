#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn output(id: &str, generated_at: i64) -> Stage1Output {
    Stage1Output {
        session_id: id.to_owned(),
        source_updated_at: generated_at,
        raw_memory: format!("raw {id}"),
        session_summary: format!("summary {id}"),
        session_slug: Some(id.to_owned()),
        generated_at,
        usage_count: None,
        last_usage: None,
    }
}

/// One worker: a job is taken once, and one that was running when the service
/// stopped is taken again after a restart rather than lost.
#[test]
fn a_job_is_taken_once_and_an_interrupted_one_runs_again_after_a_restart() {
    let store = Store::in_memory().unwrap();
    store.enqueue(EXTRACT, "s1").unwrap();
    let claim = store.claim(EXTRACT).unwrap().expect("ready");
    assert_eq!(claim.job_key, "s1");
    assert!(store.claim(EXTRACT).unwrap().is_none(), "running is not ready");
    store.enqueue(EXTRACT, "s1").unwrap();
    assert_eq!(store.status(EXTRACT, "s1").unwrap().as_deref(), Some("running"), "enqueue leaves it alone");
    assert!(!store.reopen(EXTRACT, "s1").unwrap(), "not while it runs");

    // The service stopped here. At the next start the job is ready again.
    assert_eq!(store.requeue_interrupted().unwrap(), 1);
    let again = store.claim(EXTRACT).unwrap().expect("requeued");
    store.finish(&again, &Outcome::Succeeded, 100).unwrap();
    assert_eq!(store.status(EXTRACT, "s1").unwrap().as_deref(), Some("succeeded"));
    assert!(store.claim(EXTRACT).unwrap().is_none(), "a succeeded job stays done");
}

/// A failure is final: nothing reschedules it until its source changes.
#[test]
fn a_failed_job_waits_for_its_source_to_change() {
    let store = Store::in_memory().unwrap();
    store.enqueue(CONSOLIDATE, CONSOLIDATE_KEY).unwrap();
    let claim = store.claim(CONSOLIDATE).unwrap().expect("ready");
    store.finish(&claim, &Outcome::Failed("the model said no".into()), 100).unwrap();
    assert_eq!(store.status(CONSOLIDATE, CONSOLIDATE_KEY).unwrap().as_deref(), Some("failed"));
    assert!(store.claim(CONSOLIDATE).unwrap().is_none(), "no retry loop");
    assert!(store.reopen(CONSOLIDATE, CONSOLIDATE_KEY).unwrap());
    assert!(store.claim(CONSOLIDATE).unwrap().is_some(), "a new source is a new attempt");
}

/// The loop closing: what gets cited gets kept.
#[test]
fn selection_prefers_what_was_used_and_drops_what_nothing_has_wanted() {
    let store = Store::in_memory().unwrap();
    let day = 24 * 60 * 60 * 1000;
    let now = 100 * day;

    store.put_extraction(&output("used-a-lot", now - 40 * day)).unwrap();
    store.put_extraction(&output("used-once", now - 40 * day)).unwrap();
    store.put_extraction(&output("never-used-but-fresh", now - day)).unwrap();
    store.put_extraction(&output("never-used-and-old", now - 40 * day)).unwrap();

    for i in 0..5 {
        assert!(store.record_usage("used-a-lot", &format!("citer-{i}"), now - 2 * day).unwrap());
    }
    store.record_usage("used-once", "citer-0", now - 3 * day).unwrap();

    let selected = store.select_for_consolidation(10, 30, now).unwrap();
    let ids: Vec<&str> = selected.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(
        ids,
        ["used-a-lot", "used-once", "never-used-but-fresh"],
        "most used first; something never cited is judged on its age instead, \
         and something old that nothing ever wanted falls out"
    );
    assert_eq!(selected[0].usage_count, Some(5));
}

/// Re-reading a session must not cost it the record of having been useful.
#[test]
fn a_re_extraction_replaces_the_text_and_keeps_the_usage() {
    let store = Store::in_memory().unwrap();
    store.put_extraction(&output("s1", 1_000)).unwrap();
    store.record_usage("s1", "citer", 2_000).unwrap();

    let mut again = output("s1", 5_000);
    again.session_summary = "a better summary".into();
    store.put_extraction(&again).unwrap();

    let rows = store.select_for_consolidation(10, 3650, 6_000).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_summary, "a better summary", "the text is replaced");
    assert_eq!(rows[0].usage_count, Some(1), "the usage survives");
    assert_eq!(rows[0].last_usage, Some(2_000));
}

/// What a session can have used is what a consolidation has consumed, and
/// nothing else.
#[test]
fn only_consolidated_rows_are_offered_as_what_a_session_could_have_used() {
    let store = Store::in_memory().unwrap();
    store.put_extraction(&output("s1", 1_000)).unwrap();
    store.put_extraction(&output("s2", 1_000)).unwrap();
    assert!(store.selected().unwrap().is_empty(), "nothing has been consolidated yet");

    store.mark_consolidated(&[output("s1", 1_000)]).unwrap();
    let selected = store.selected().unwrap();
    assert_eq!(selected.iter().map(|r| r.session_id.clone()).collect::<Vec<_>>(), ["s1"]);
}

/// A count is what decides whether a memory survives, so a conversation that
/// keeps citing the same line must not be able to keep it alive by itself.
#[test]
fn one_session_citing_a_memory_ten_times_has_used_it_once() {
    let store = Store::in_memory().unwrap();
    store.put_extraction(&output("m1", 1_000)).unwrap();
    assert!(store.record_usage("m1", "chat-a", 2_000).unwrap(), "the first time counts");
    for turn in 0..9 {
        assert!(!store.record_usage("m1", "chat-a", 2_100 + turn).unwrap(), "and the rest do not");
    }
    assert!(store.record_usage("m1", "chat-b", 3_000).unwrap(), "another session is another use");

    let rows = store.select_for_consolidation(10, 3650, 4_000).unwrap();
    assert_eq!(rows[0].usage_count, Some(2), "two sessions, eleven citations");
    assert_eq!(rows[0].last_usage, Some(3_000));
}
