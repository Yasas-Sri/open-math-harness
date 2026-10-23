#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::mpsc as std_mpsc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::*;

/// An inbox whose jobs a thread of the test answers, or keeps unanswered.
fn inbox(answering: bool) -> Inbox {
    let (jobs, requests) = std_mpsc::channel::<Job>();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        for job in requests {
            if answering {
                drop(job.answer.send(respond(Some(job.id), Ok(json!({"op": "done"})))));
            } else {
                kept.push(job);
            }
        }
    });
    Inbox::new(jobs, Arc::new(|| true))
}

#[test]
fn an_answer_over_the_line_bound_is_refused_by_its_id_never_cut() {
    let small = json!({"id": 7, "ok": {"text": "hi"}});
    assert_eq!(encode(&small, MAX_ANSWER_BYTES), format!("{small}\n").into_bytes());
    let big = json!({"id": 8, "ok": {"text": "x".repeat(MAX_ANSWER_BYTES)}});
    let line = encode(&big, MAX_ANSWER_BYTES);
    assert!(line.len() < 1024 && line.ends_with(b"\n"));
    let refused: Value = serde_json::from_slice(&line).unwrap();
    assert_eq!(
        (refused["id"].as_u64(), refused["error"]["code"].as_str()),
        (Some(8), Some("limit_exceeded"))
    );
}

/// A full history page, at the conversation service's page target, fits an
/// ordinary answer line with the largest id; a page holding one record past
/// the target (which the service answers alone) gets the history bound, which
/// only the answer to a `conversation.history` request has.
#[tokio::test]
async fn a_full_history_page_fits_its_line_and_a_single_large_record_its_own_bound() {
    use nucleus_conversation_interface::{HISTORY_PAGE_BYTES, MAX_HISTORY_PAYLOAD_BYTES};
    // A page exactly at the target: `{"records":[…],"has_more":false}`.
    let frame = serde_json::to_vec(&json!({"records": [], "has_more": false})).unwrap().len();
    let text =
        "x".repeat(HISTORY_PAGE_BYTES - frame - serde_json::to_vec(&json!({"text": ""})).unwrap().len());
    let page = json!({"records": [{"text": text}], "has_more": false});
    assert_eq!(serde_json::to_vec(&page).unwrap().len(), HISTORY_PAGE_BYTES);
    let full = json!({"id": u64::MAX, "ok": page});
    let line = encode(&full, MAX_ANSWER_BYTES);
    assert!(line.len() <= MAX_ANSWER_BYTES + 1, "{}", line.len());
    assert_eq!(serde_json::from_slice::<Value>(&line).unwrap(), full, "never cut, never refused");

    let (mut window, _answers) = Window::new(Peer::unlabelled(), inbox(false));
    let one_record =
        |id: u64, bytes: usize| json!({"id": id, "ok": {"records": [{"text": "y".repeat(bytes)}]}});
    window.request(br#"{"id":1,"op":"conversation.history","session":"s"}"#);
    window.request(br#"{"id":2,"op":"sessions.list"}"#);
    window.request(br#"{"id":3,"op":"conversation.history","session":"s"}"#);
    let large = window.encode(&one_record(1, 2 * MAX_ANSWER_BYTES));
    assert!(
        serde_json::from_slice::<Value>(&large).unwrap()["ok"].is_object(),
        "the history page goes whole"
    );
    let elsewhere = window.encode(&one_record(2, 2 * MAX_ANSWER_BYTES));
    assert_eq!(serde_json::from_slice::<Value>(&elsewhere).unwrap()["error"]["code"], "limit_exceeded");
    let too_large = window.encode(&one_record(3, MAX_HISTORY_PAYLOAD_BYTES + 64));
    assert_eq!(serde_json::from_slice::<Value>(&too_large).unwrap()["error"]["code"], "limit_exceeded");
    // Each history request's answer had the bound once.
    let again = window.encode(&one_record(1, 2 * MAX_ANSWER_BYTES));
    assert_eq!(serde_json::from_slice::<Value>(&again).unwrap()["error"]["code"], "limit_exceeded");
}

/// A window has [`MAX_IN_FLIGHT`] requests unanswered at most: one more is
/// answered at once with `limit_exceeded`, by its id; and one that keeps
/// sending without reading its answers is not read on.
#[tokio::test]
async fn a_window_past_its_requests_in_flight_hears_why() {
    let (mut window, mut answers) = Window::new(Peer::unlabelled(), inbox(false));
    for id in 0..MAX_IN_FLIGHT {
        assert!(window.request(format!(r#"{{"id":{id},"op":"sessions.list"}}"#).as_bytes()));
    }
    assert!(answers.try_recv().is_err(), "nothing answered yet");
    assert!(window.request(br#"{"id":99,"op":"sessions.list"}"#));
    let refused = answers.recv().await.unwrap();
    assert_eq!(
        (refused["id"].as_u64(), refused["error"]["code"].as_str()),
        (Some(99), Some("limit_exceeded"))
    );
    assert!(window.reading());
    for id in 100..100 + MAX_IN_FLIGHT {
        window.request(format!(r#"{{"id":{id},"op":"sessions.list"}}"#).as_bytes());
    }
    assert!(!window.reading(), "not read on while its answers are not read");
    window.written();
    window.written();
    assert!(window.reading(), "read on once fewer than twice its bound wait");
}

#[tokio::test]
async fn lines_are_read_whole_and_within_their_bound() {
    let (mut near, far) = tokio::io::duplex(64);
    let mut reader = tokio::io::BufReader::with_capacity(16, far);
    tokio::spawn(async move {
        near.write_all(b"{\"a\":1}\n").await.unwrap();
        near.write_all(&[b'x'; 40]).await.unwrap();
        near.write_all(b"\n").await.unwrap();
        near.write_all(&[b'y'; 41]).await.unwrap();
        near.write_all(b"\n").await.unwrap();
    });
    assert!(matches!(read_line(&mut reader, 40).await.unwrap(), Line::Whole(line) if line == b"{\"a\":1}"));
    assert!(matches!(read_line(&mut reader, 40).await.unwrap(), Line::Whole(line) if line.len() == 40));
    assert!(matches!(read_line(&mut reader, 40).await.unwrap(), Line::TooLong));
}

/// The 65th window is served like the first: windows are tasks, and there is
/// no limit on them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_window_is_served_however_many_there_are() {
    let inbox = inbox(true);
    let mut clients = Vec::new();
    for _ in 0..200 {
        let (near, far) = UnixStream::pair().unwrap();
        tokio::spawn(socket_window(far, false, inbox.clone()));
        clients.push(near);
    }
    for (n, client) in clients.iter_mut().enumerate() {
        client.write_all(format!("{{\"id\":{n},\"op\":\"sessions.list\"}}\n").as_bytes()).await.unwrap();
    }
    for (n, client) in clients.into_iter().enumerate() {
        let mut lines = tokio::io::BufReader::new(client).lines();
        let answer: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            (answer["id"].as_u64(), &answer["ok"]),
            (Some(u64::try_from(n).unwrap()), &json!({"op": "done"}))
        );
    }
}

/// On a labelled socket the first line is the label: one that is not, or is
/// longer than 4 KiB, is answered with why and the connection ends; a
/// request line longer than its bound too, never cut.
#[tokio::test]
async fn a_connection_that_breaks_a_bound_hears_why_and_ends() {
    let label = json!({"v": 1, "kind": "window", "account": "a-1", "nickname": "Ada", "role": "member"});
    let long = format!(r#"{},"x":"{}"}}"#, label.to_string().trim_end_matches('}'), "a".repeat(5000));
    for (first, then, why) in [
        (r#"{"member":"Ada"}"#.to_owned(), String::new(), "label"),
        (long, String::new(), "4096"),
        (label.to_string(), "x".repeat(MAX_REQUEST_BYTES + 1), "at most"),
    ] {
        let (mut near, far) = UnixStream::pair().unwrap();
        tokio::spawn(socket_window(far, true, inbox(true)));
        near.write_all(format!("{first}\n").as_bytes()).await.unwrap();
        if !then.is_empty() {
            // The connection may end before all of it is written.
            drop(near.write_all(format!("{then}\n").as_bytes()).await);
        }
        let mut lines = tokio::io::BufReader::new(near).lines();
        let refusal: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(refusal["error"]["code"], "invalid_input", "{why}: {refusal}");
        assert!(refusal["error"]["message"].as_str().unwrap().contains(why), "{why}: {refusal}");
        assert!(lines.next_line().await.unwrap_or(None).is_none(), "{why}: the connection ends");
    }
}
