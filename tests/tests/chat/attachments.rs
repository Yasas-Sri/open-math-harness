//! Files a person attaches to a message reach the model: an image as an image,
//! a PDF (through the session's `pdf` tool) and a text file as text, each named
//! with its path in the working directory. A file the agent cannot use is named
//! with the reason and shown as an error, and the turn goes on. An image
//! `view_image` reads follows the tool results, in a user message of its own.
//! An image the message carries inline comes first, by its name. An image of
//! the working directory the answer references goes into its record for the
//! person, and the model is told, once, which it could not show.

use super::*;

/// A 1×1 PNG.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\nIDATx\x9cc\0\x01\0\0\x05\0\x01\r\n-\xb4\0\0\0\0IEND\xaeB`\x82";
/// A 1×1 GIF.
const GIF: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff!\xf9\x04\x01\0\0\0\0,\0\0\0\0\x01\0\x01\0\0\x02\x02D\x01\0;";

/// A valid PDF with one page per text, each drawing its text.
fn pdf(texts: &[&str]) -> Vec<u8> {
    let kids: Vec<String> = (0..texts.len()).map(|i| format!("{} 0 R", 4 + 2 * i)).collect();
    let mut objects = vec![
        "<</Type/Catalog/Pages 2 0 R>>".to_owned(),
        format!("<</Type/Pages/Kids[{}]/Count {}>>", kids.join(" "), texts.len()),
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_owned(),
    ];
    for (i, text) in texts.iter().enumerate() {
        let content = format!("BT /F1 24 Tf 20 100 Td ({text}) Tj ET");
        objects.push(format!(
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 144]/Contents {} 0 R/Resources<</Font<</F1 3 0 R>>>>>>",
            5 + 2 * i
        ));
        objects.push(format!("<</Length {}>>stream\n{content}\nendstream", content.len()));
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj{object}endobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for offset in offsets {
        out.push_str(&format!("{offset:010} 00000 n \n"));
    }
    out.push_str(&format!("trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF", objects.len() + 1));
    out.into_bytes()
}

/// The `data:` URL of an image part, decoded.
fn image_bytes(part: &Value) -> Vec<u8> {
    use base64::Engine;
    let url = part["image_url"]["url"].as_str().unwrap();
    let (_, data) = url.split_once(";base64,").unwrap();
    base64::engine::general_purpose::STANDARD.decode(data).unwrap()
}

#[test]
fn attached_files_and_viewed_images_reach_the_model_and_unusable_ones_are_named() {
    let server = FakeHttp::start();
    let mut scenario = Scenario::new();
    let attached = |name: &str| format!("work/attachments/default/{name}");
    let write = |scenario: &Scenario, relative: &str, bytes: &[u8]| {
        let path = scenario.host(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    write(&scenario, "work/fig.gif", GIF);
    let figure = [PNG, b"the reply's own"].concat();
    write(&scenario, "work/figures/sinc.png", &figure);
    write(&scenario, &attached("plot.png"), PNG);
    write(&scenario, &attached("notes.md"), b"# Notes\nThe answer is 42.\n");
    let paper = pdf(&["Lemma one", "Proof two"]);
    write(&scenario, &attached("paper.pdf"), &paper);
    write(&scenario, &attached("fake.png"), b"not an image at all");
    std::os::unix::fs::symlink("plot.png", scenario.host(&attached("linked.png"))).unwrap();
    let big = [PNG, &vec![0; nucleus_model_interface::MAX_IMAGE_BYTES]].concat();
    write(&scenario, &attached("big.png"), &big);
    write(&scenario, &attached("changed.png"), PNG);

    scenario.add_openrouter("openrouter", &server.base_url(), KEY, &["--retries", "0"], true);
    scenario.add_conversation("conversation");
    scenario.add_pdf("pdf", "work");
    let pdf_tool = json!([{"service": "pdf", "endpoint": "pdf", "env": "compute", "workspace": "default",
                           "resource": []}])
    .to_string();
    let args =
        ["--max-steps", "8", "--workdir", "/work", "--image-model", MODEL, "--tool-bindings", &pdf_tool];
    scenario.add_chat_agent("chat", MODEL, &args);
    scenario.bind("chat", "work", "/work", false);
    scenario.add_sessions("sessions", "chat");
    scenario.add_ui("ui");
    let mut supervisor =
        scenario.start_with(scenario.config(|limits| limits.max_inflight_per_endpoint = 16, 64)).unwrap();
    for (name, endpoint) in [("openrouter", "model"), ("conversation", "conversation"), ("pdf", "pdf")] {
        scenario.boot_service(&supervisor, name, endpoint);
    }
    let mut grants = sessions_grants();
    grants.push(cap("pdf", &["extract"], &[]));
    let sessions = supervisor.boot(program("sessions"), grants).unwrap();
    let target = nucleus_e2e::endpoint("sessions");
    assert!(supervisor.wait_until(TIMEOUT, |s| s.provider_of(&target).is_some_and(|(p, _)| p == sessions)));
    server.script(Scripted::json(
        200,
        &tool_request(MODEL, &[("v1", "view_image", json!({"path": "/work/fig.gif"}))]),
    ));
    let answer = "A plot, a paper and notes. ![sinc](figures/sinc.png) ![big](attachments/default/big.png) \
        ![gone](gone.png) `![code](fig.gif)` ![web](https://example.org/x.png)";
    server.script(Scripted::json(200, &completion(MODEL, answer)));
    server.script(Scripted::json(200, &completion(MODEL, "Smaller now.")));
    supervisor.boot(program("ui"), ui_grants(&[], true)).unwrap();
    let mut ui = scenario.connect_ui("ui");
    ui.request(json!({"op": "sessions.create", "session": "default"})).unwrap();

    let file = |name: &str, media_type: &str, bytes: usize| json!({"name": name, "media_type": media_type, "path": format!("attachments/default/{name}"), "bytes": bytes});
    let files = [
        file("plot.png", "image/png", PNG.len()),
        file("notes.md", "text/markdown", 26),
        file("paper.pdf", "application/pdf", paper.len()),
        file("fake.png", "image/png", 19),
        file("linked.png", "image/png", PNG.len()),
        file("big.png", "image/png", big.len()),
        file("changed.png", "image/png", PNG.len() - 1),
    ];
    let sketch = {
        use base64::Engine;
        json!({"name": "sketch.gif", "media_type": "image/gif",
               "data_base64": base64::engine::general_purpose::STANDARD.encode(GIF)})
    };
    let say = json!({"op": "conversation.say", "session": "default", "message_id": "u1",
                     "text": "What are these?", "files": files, "images": [sketch]});
    ui.request(say).unwrap();
    let log = ui.wait_turns("default", 1);

    // The person's message: the note naming its image and every file, the
    // text of the text file and of the PDF page by page, then the images
    // after their labels, the one it carries first.
    let asked = sent_messages(&server, 0).last().unwrap().clone();
    assert_eq!(asked["role"], "user");
    let parts = asked["content"].as_array().unwrap();
    let text = parts[0]["text"].as_str().unwrap();
    assert!(text.starts_with("What are these?"), "{text}");
    for (path, outcome) in [
        ("plot.png", "shown below"),
        ("notes.md", "its text is below"),
        ("paper.pdf", "2 pages): its text is below"),
        ("fake.png", "not shown: it is not a PNG, JPEG, WebP or GIF image"),
        ("linked.png", "not shown: a symbolic link is in its path"),
        ("big.png", "not shown: it is larger than 4.0 MiB"),
        ("changed.png", "not shown: it changed after it was attached"),
    ] {
        let line = text.lines().find(|l| l.starts_with(&format!("- attachments/default/{path} "))).unwrap();
        assert!(line.contains(outcome), "{line}");
    }
    assert!(text.contains("- \"sketch.gif\" (image/gif, ") && text.contains("): shown below"), "{text}");
    assert!(text.contains("The answer is 42."), "{text}");
    let (one, two) = (text.find("[page 1 of 2]").unwrap(), text.find("[page 2 of 2]").unwrap());
    assert!(text[one..two].contains("Lemma one") && text[two..].contains("Proof two"), "{text}");
    assert_eq!(parts.len(), 5, "the text, then each image after its label: {parts:?}");
    assert!(
        parts[1]["text"].as_str().unwrap().starts_with("\"sketch.gif\" (sent with the message, image/gif")
    );
    assert_eq!(image_bytes(&parts[2]), GIF);
    assert!(parts[3]["text"].as_str().unwrap().starts_with("attachments/default/plot.png (image/png"));
    assert_eq!(image_bytes(&parts[4]), PNG);

    // What it could not use is said in the conversation; the turn went on.
    let errors: Vec<&str> =
        log.iter().filter(|r| r["kind"] == "error").map(|r| r["text"].as_str().unwrap()).collect();
    assert_eq!(errors.len(), 4, "{errors:?}");
    for name in ["fake.png", "linked.png", "big.png", "changed.png"] {
        assert!(
            errors.iter().any(|e| e.starts_with(&format!("attachments/default/{name} was not shown"))),
            "{errors:?}"
        );
    }
    assert_eq!(log.last().unwrap()["kind"], "turn_complete");

    // The viewed image follows the tool results, which stay text.
    let next = sent_messages(&server, 1);
    let call =
        next.iter().position(|m| m["role"] == "assistant" && m["tool_calls"][0]["id"] == "v1").unwrap();
    assert_eq!(next[call + 1]["role"], "tool");
    assert!(next[call + 1]["content"].as_str().unwrap().contains("fig.gif"));
    let viewed = next[call + 2]["content"].as_array().unwrap();
    assert_eq!(next[call + 2]["role"], "user");
    assert!(viewed.iter().any(|p| p["text"].as_str().is_some_and(|t| t.starts_with("fig.gif (image/gif"))));
    assert_eq!(image_bytes(viewed.last().unwrap()), GIF);
    assert_eq!(image_bytes(&next[call - 1]["content"][4]), PNG, "the person's images are still in view");

    // The answer's record shows the person the image it references beneath
    // the working directory and names those it could not show; one in code
    // and a URL are no references it reads.
    let instructions = sent_messages(&server, 0)[0]["content"].as_str().unwrap().to_owned();
    assert!(instructions.contains("`![caption](relative/path.png)`"), "{instructions}");
    let shown = log.iter().find(|r| r["kind"] == "assistant" && r["text"] == answer).unwrap();
    let encoded = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&figure)
    };
    assert_eq!(
        shown["images"],
        json!([{"path": "figures/sinc.png", "media_type": "image/png", "data_base64": encoded}])
    );
    assert_eq!(
        shown["images_refused"],
        json!([{"path": "attachments/default/big.png", "reason": "too_large", "bytes": big.len()},
               {"path": "gone.png", "reason": "not_found"}])
    );
    // The model's next request is told, once, what was not shown, and is
    // not shown the image.
    let more = json!({"op": "conversation.say", "session": "default", "message_id": "u2",
                      "text": "And the big one?"});
    ui.request(more).unwrap();
    ui.wait_turns("default", 2);
    let asked = sent_messages(&server, 2);
    let told = asked.last().unwrap()["content"].as_str().unwrap();
    assert!(told.starts_with("[Images your reply referenced"), "{told}");
    assert!(
        told.contains("- attachments/default/big.png: too_large") && told.contains("- gone.png: not_found")
    );
    assert!(told.ends_with("And the big one?") && !told.contains("sinc.png"), "{told}");
    let body = serde_json::to_string(&asked).unwrap();
    assert_eq!(body.matches("[Images your reply referenced").count(), 1);
    assert!(!body.contains(&encoded));

    drop(ui);
    assert!(supervisor.shutdown().is_clean());
}
