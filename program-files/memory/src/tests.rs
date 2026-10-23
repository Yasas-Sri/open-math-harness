#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn parse(args: &[&str]) -> Result<Options, String> {
    Options::parse(args.iter().map(|s| (*s).to_owned()))
}

fn base() -> Vec<&'static str> {
    vec!["--root", "/base/memories", "--model", "x/y", "--agent-executable", "/programs/chat"]
}

#[test]
fn the_three_required_options_are_required() {
    for drop in ["--root", "--model", "--agent-executable"] {
        let mut args = base();
        let at = args.iter().position(|a| *a == drop).unwrap();
        args.drain(at..at + 2);
        assert!(parse(&args).is_err(), "{drop} should be required");
    }
    let options = parse(&base()).unwrap();
    assert_eq!(options.agent_model(), "x/y", "the agent runs on the extraction model unless told otherwise");
}

/// The job store holds the baseline; inside the workspace, the `fs` service
/// that serves the memory directory could reach it.
#[test]
fn the_job_store_may_not_live_inside_the_workspace() {
    let mut args = base();
    args.extend(["--db", "/base/memories/jobs"]);
    let e = parse(&args).unwrap_err();
    assert!(e.contains("inside the workspace"), "{e}");

    let options = parse(&base()).unwrap();
    assert_eq!(options.db_path().unwrap(), PathBuf::from("/base/memory-jobs"));
}

#[test]
fn bookkeeping_records_are_not_part_of_the_conversation_a_model_reads() {
    let records = vec![
        json!({"kind": "user", "seq": 1, "text": "do the thing"}),
        json!({"kind": "model_call", "seq": 2, "input_tokens": 9}),
        json!({"kind": "assistant_delta", "seq": 3, "message_id": "m", "text": "par"}),
        json!({"kind": "tool_call", "seq": 4, "name": "fs_read", "arguments": {"path": "a"}}),
        json!({"kind": "tool_result", "seq": 5, "error_code": "permission_denied", "summary": "no"}),
        json!({"kind": "assistant", "seq": 6, "text": "done"}),
        json!({"kind": "turn_complete", "seq": 7}),
    ];
    let lines = lines_of(&records);
    assert_eq!(
        lines,
        [
            "user: do the thing".to_owned(),
            "tool call fs_read: {\"path\":\"a\"}".to_owned(),
            "tool result [permission_denied]: no".to_owned(),
            "assistant: done".to_owned(),
        ]
    );
}

/// A kind this service has not heard of is more likely to be something that
/// was said than something that was counted.
#[test]
fn an_unknown_record_kind_is_kept_rather_than_dropped() {
    let lines = lines_of(&[json!({"kind": "whisper", "seq": 1, "text": "hello"})]);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("whisper: "), "{}", lines[0]);
}

/// Both ends of a session matter: the start says what was asked for and the
/// end says how it turned out.
#[test]
fn a_session_that_does_not_fit_loses_its_middle_and_says_so() {
    let lines: Vec<String> =
        (0..200).map(|i| format!("user: message number {i} with some padding")).collect();
    let whole = fit(&lines, 100_000);
    assert!(whole.contains("number 0") && whole.contains("number 199"));
    assert!(!whole.contains("omitted"));

    let cut = fit(&lines, 1_000);
    assert!(cut.len() <= 1_200, "cut to {} bytes", cut.len());
    assert!(cut.contains("number 0"), "the start is kept");
    assert!(cut.contains("number 199"), "the end is kept");
    assert!(cut.contains("records omitted here"), "and the gap says so");
}

#[test]
fn history_reads_continue_after_byte_bounded_short_pages_and_stop_at_explicit_eof() {
    let mut cursors = Vec::new();
    let mut pages = std::collections::VecDeque::from([
        conversation::HistoryResponse { records: vec![json!({"seq":0}), json!({"seq":2})], has_more: true },
        conversation::HistoryResponse { records: vec![json!({"seq":7})], has_more: false },
    ]);
    let records = read_history_pages(|after| {
        cursors.push(after);
        pages.pop_front().ok_or_else(|| "read beyond explicit EOF".into())
    })
    .unwrap();
    assert_eq!(cursors, [None, Some(2)]);
    assert_eq!(records, [json!({"seq":0}), json!({"seq":2}), json!({"seq":7})]);
    let full = (0..conversation::MAX_HISTORY_RECORDS).map(|seq| json!({"seq":seq})).collect::<Vec<_>>();
    let mut calls = 0;
    let records = read_history_pages(|_| {
        calls += 1;
        Ok(conversation::HistoryResponse { records: full.clone(), has_more: false })
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(records, full);
}

#[test]
fn history_reads_reject_nonadvancing_or_failed_continuations() {
    for tail in [
        Ok(conversation::HistoryResponse { records: Vec::new(), has_more: true }),
        Ok(conversation::HistoryResponse { records: vec![json!({"seq":3})], has_more: true }),
        Err("provider disappeared".to_owned()),
    ] {
        let mut calls = 0;
        let result = read_history_pages(|after| {
            calls += 1;
            if after.is_none() {
                Ok(conversation::HistoryResponse { records: vec![json!({"seq":3})], has_more: true })
            } else {
                tail.clone()
            }
        });
        assert!(result.is_err(), "a partial history must not be accepted as complete");
        assert_eq!(calls, 2, "invalid continuation cannot loop");
    }
}
