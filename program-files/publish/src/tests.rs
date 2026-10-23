#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use nucleus_publish_interface::MAX_RECORD_FINDINGS;
use serde_json::{Value, json};

use super::*;

const SALT: &str = "00112233445566778899aabbccddeeff";
const TOOL_KEY: &str = "sk-proj-Ab12Cd34Ef56Gh78Ij90Kl";

/// A session's log as the conversation service pages it: `page` records at
/// a time, from `after`.
struct Fake {
    log: Vec<Value>,
    page: usize,
}

impl Sources for Fake {
    fn history(&mut self, resource: &ResourcePath, after: Option<u64>) -> Result<HistoryResponse, Error> {
        assert_eq!(resource.segments(), ["default", "s1"]);
        let rest: Vec<Value> = self
            .log
            .iter()
            .filter(|r| after.is_none_or(|after| r["seq"].as_u64().unwrap() > after))
            .cloned()
            .collect();
        let has_more = rest.len() > self.page;
        Ok(HistoryResponse { records: rest.into_iter().take(self.page).collect(), has_more })
    }
}

fn session() -> ResourcePath {
    ResourcePath::new(["default", "s1"]).unwrap()
}

fn call(sources: &mut Fake, after: Option<u64>) -> Result<Findings, Error> {
    let payload = Payload::encode(&json!({"salt": SALT, "after": after})).unwrap();
    handle(sources, publish::ACTION_FINDINGS, &session(), &payload)?.decode()
}

fn record(seq: u64, value: Value) -> Value {
    let mut value = value;
    value["seq"] = json!(seq);
    value["at_ms"] = json!(1000 + seq);
    value
}

/// Each record is scanned whole, every string and every object as its JSON:
/// the findings come by `seq`, the secret masked in the preview and named
/// only by its salted fingerprint, a record with none is not listed, and
/// the page says which records it covered.
#[test]
fn findings_come_by_seq_without_the_secret() {
    let summary = format!("HOME=/home/default\nOPENAI_API_KEY={TOOL_KEY}\nLANG=C\n");
    let log = vec![
        record(0, json!({"kind": "user", "text": "deploy it", "author": "Ada"})),
        record(
            1,
            json!({"kind": "tool_call", "call_id": "c1", "name": "shell_exec", "arguments": {"password": "hunter2hunter2"}}),
        ),
        record(2, json!({"kind": "tool_result", "call_id": "c1", "summary": summary})),
        record(3, json!({"kind": "assistant", "text": "Done."})),
        record(4, json!({"kind": "turn_complete"})),
    ];
    let mut sources = Fake { log, page: 500 };
    let page = call(&mut sources, None).unwrap();
    let text = serde_json::to_string(&page).unwrap();
    for secret in [TOOL_KEY, "hunter2hunter2"] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    let seqs: Vec<u64> = page.records.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, [1, 2]);
    assert_eq!((page.through, page.next, page.rules), (Some(4), None, RULES_VERSION));
    let rules =
        |seq: usize| -> Vec<&str> { page.records[seq].findings.iter().map(|f| f.rule.as_str()).collect() };
    assert_eq!(rules(0), ["secret-assignment"], "the arguments as their JSON");
    assert!(rules(1).contains(&"sk-key"), "{:?}", rules(1));
    let finding = &page.records[1].findings.iter().find(|f| f.rule == "sk-key").unwrap();
    assert!(finding.preview.contains("OPENAI_API_KEY=****"), "{}", finding.preview);
    // The fingerprint is the salt's, the rule's and the secret's, so the
    // platform releases one secret wherever it is, and it says nothing to
    // whoever lacks the salt.
    let fingerprint =
        |salt: &str| Scanner { salt: salt.into() }.record(&json!({"t": TOOL_KEY})).0[0].fingerprint.clone();
    assert_eq!(finding.fingerprint, fingerprint(SALT));
    assert_ne!(fingerprint(SALT), fingerprint(&"ff".repeat(16)));
    // An image a message carries is read by its name and type, never its bytes.
    let image = |name: &str, data: &str| json!({"kind": "user", "text": "", "images": [{"name": name, "media_type": "image/png", "data_base64": data}]});
    let scanner = Scanner { salt: SALT.into() };
    assert_eq!(scanner.record(&image("shot.png", &format!("iVBO {TOOL_KEY}"))).0, []);
    assert_eq!(scanner.record(&image(&format!("{TOOL_KEY} shot.png"), "iVBO")).0[0].rule, "sk-key");
    // So is an image of the project a reply shows, by its path and type; what
    // it names as not shown is read whole.
    let reply = |path: &str, data: &str, refused: &str| {
        json!({"kind": "assistant", "text": "",
               "images": [{"path": path, "media_type": "image/png", "data_base64": data}],
               "images_refused": [{"path": refused, "reason": "not_found"}]})
    };
    assert_eq!(scanner.record(&reply("a.png", &format!("iVBO {TOOL_KEY}"), "b.png")).0, []);
    for (path, refused) in
        [(format!("{TOOL_KEY}.png"), "b.png".to_owned()), ("a.png".into(), format!("{TOOL_KEY}.png"))]
    {
        assert_eq!(scanner.record(&reply(&path, "iVBO", &refused)).0[0].rule, "sk-key", "{path} {refused}");
    }
    // From where the platform stopped, only what is new.
    let empty = call(&mut sources, Some(4)).unwrap();
    assert_eq!((empty.records.len(), empty.through, empty.next), (0, None, None));
}

/// A session's findings are read page by page as its log is: `next` until
/// the end, every record covered once; and a page ends early, at a whole
/// record, rather than answer more findings than a page may.
#[test]
fn findings_are_paged_with_the_log_and_bounded() {
    let secret = |n: u64, k: u64| format!("sk-{n:04}x{k:03}Ab12Cd34Ef56Gh78Ij90Kl");
    let line = |n: u64| (0..MAX_RECORD_FINDINGS as u64).map(|k| secret(n, k)).collect::<Vec<_>>().join(" ");
    let log: Vec<Value> = (0..70).map(|n| record(n, json!({"kind": "assistant", "text": line(n)}))).collect();
    let mut sources = Fake { log, page: 500 };
    let (mut seen, mut after, mut pages) = (Vec::new(), None, 0);
    loop {
        let page = call(&mut sources, after).unwrap();
        pages += 1;
        assert!(page.records.iter().map(|r| r.findings.len()).sum::<usize>() <= MAX_PAGE_FINDINGS);
        assert!(page.records.iter().all(|r| r.findings.len() == MAX_RECORD_FINDINGS && !r.more));
        seen.extend(page.records.iter().map(|r| r.seq));
        assert_eq!(page.through, page.records.last().map(|r| r.seq));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!((pages, seen), (2, (0..70).collect::<Vec<u64>>()));
    // A short history page is not the end of the log.
    let mut short = Fake { page: 10, ..sources };
    let first = call(&mut short, None).unwrap();
    assert_eq!((first.through, first.next), (Some(9), Some(9)));
}

/// A record with more distinct findings than one lists says so: it is
/// never answered as if what is listed were all of it.
#[test]
fn a_record_with_more_findings_than_listed_says_so() {
    let many: Vec<String> =
        (0..MAX_RECORD_FINDINGS + 3).map(|k| format!("sk-{k:04}Ab12Cd34Ef56Gh78Ij90Kl")).collect();
    let log = vec![record(0, json!({"kind": "assistant", "text": many.join("\n")}))];
    let page = call(&mut Fake { log, page: 500 }, None).unwrap();
    assert_eq!((page.records[0].findings.len(), page.records[0].more), (MAX_RECORD_FINDINGS, true));
}

#[test]
fn a_call_names_one_session_and_a_salt() {
    let mut sources = Fake { log: Vec::new(), page: 500 };
    let payload = |value: Value| Payload::encode(&value).unwrap();
    for (resource, request) in [
        (ResourcePath::new(["default"]).unwrap(), json!({"salt": SALT})),
        (ResourcePath::new(["Default", "s1"]).unwrap(), json!({"salt": SALT})),
        (session(), json!({"salt": "short"})),
        (session(), json!({"salt": SALT, "blocks": []})),
    ] {
        let refused = handle(&mut sources, publish::ACTION_FINDINGS, &resource, &payload(request.clone()));
        assert_eq!(refused.unwrap_err().code, ErrorCode::InvalidInput, "{resource} {request}");
    }
}
