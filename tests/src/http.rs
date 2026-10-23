//! A scripted HTTP/1.1 server standing in for OpenRouter. It answers each
//! request with the next scripted response and remembers what it was sent, so
//! tests can check exactly what left the adapter. Connections are served
//! concurrently, one thread each, so a test can see how many requests the
//! adapter has in flight at once; the scripts are consumed in the order the
//! requests were read. Every wait in a reply (a delay, a gap, a gate) ends
//! when the server is dropped, so a held reply never outlives its test.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What the server received.
#[derive(Clone, Debug)]
pub struct Received {
    pub method: String,
    pub path: String,
    /// Header names are lower-cased.
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// What the server will answer.
#[derive(Clone, Debug)]
pub struct Scripted {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub delay: Duration,
    /// When set, the body is sent as a server-sent event stream: one event per
    /// entry, `gap` apart, then the connection closes.
    pub events: Option<(Vec<String>, Duration)>,
    /// When set, nothing of the reply is sent until this is released (see [`Self::held`]).
    hold: Option<Arc<AtomicBool>>,
}

impl Scripted {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string(),
            delay: Duration::ZERO,
            events: None,
            hold: None,
        }
    }

    /// A `200` event stream of exactly `events`, `gap` apart.
    pub fn events(events: Vec<String>, gap: Duration) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: String::new(),
            delay: Duration::ZERO,
            events: Some((events, gap)),
            hold: None,
        }
    }

    /// A `302`: the kind an HTTP client follows by default even for a POST
    /// (as a GET), which is what makes it a meaningful test of refusal.
    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".to_owned(), location.to_owned())],
            body: String::new(),
            delay: Duration::ZERO,
            events: None,
            hold: None,
        }
    }

    /// A stream that delivers `pieces` and then just stops: no finish reason,
    /// no usage, no `[DONE]`. What a consumer sees when the connection dies
    /// mid-reply, and the case a retry must NOT repeat, because the text is
    /// already in the puller's hands.
    pub fn cut_stream(model: &str, pieces: &[&str], gap: Duration) -> Self {
        let events = pieces
            .iter()
            .map(|piece| {
                serde_json::json!({"model": model, "choices": [{"delta": {"content": piece}}]}).to_string()
            })
            .collect();
        Self {
            headers: vec![("Content-Type".to_owned(), "text/event-stream".to_owned())],
            ..Self::events(events, gap)
        }
    }

    /// A streamed chat completion: `pieces` of text, then a finish reason,
    /// usage, and `[DONE]`, each `gap` apart.
    pub fn stream(model: &str, pieces: &[&str], gap: Duration) -> Self {
        let mut events: Vec<String> = pieces
            .iter()
            .map(|piece| {
                serde_json::json!({"model": model, "choices": [{"delta": {"content": piece}}]}).to_string()
            })
            .collect();
        events.push(serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}).to_string());
        // `cost` is there because the adapter asks for it; a caller must get
        // the provider's own figure, not one derived from these counts.
        events.push(
            serde_json::json!({"choices": [], "usage": {
                "prompt_tokens": 9, "completion_tokens": pieces.len(), "cost": 0.000_018
            }})
            .to_string(),
        );
        events.push("[DONE]".to_owned());
        Self::events(events, gap)
    }

    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Holds this reply, before any of it is sent, until the returned gate is
    /// released or dropped, or the test ends: a model call that is still
    /// running for as long as the test needs it to be.
    pub fn held(mut self) -> (Self, StreamGate) {
        let released = Arc::new(AtomicBool::new(false));
        self.hold = Some(Arc::clone(&released));
        (self, StreamGate(released))
    }
}

type ResponseFactory = Box<dyn FnOnce(&Received) -> Scripted + Send>;

/// Releases a held reply ([`Scripted::held`]) or a scripted stream after its
/// first event ([`FakeHttp::script_gated_stream`]). Dropping the guard also
/// releases it.
pub struct StreamGate(Arc<AtomicBool>);

impl StreamGate {
    pub fn release(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Drop for StreamGate {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Default)]
struct Shared {
    script: Mutex<VecDeque<Scripted>>,
    /// Per-key scripts (see [`FakeHttp::script_for`]), in the order the keys
    /// were first scripted.
    keyed: Mutex<Vec<(String, VecDeque<ResponseFactory>)>>,
    received: Mutex<Vec<Received>>,
    stream_gates: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    /// The server was dropped: every reply still waiting gives up.
    stop: AtomicBool,
}

/// Waits until `gate` (if any) is released and `delay` has passed; false when
/// the server stopped first.
fn wait(shared: &Shared, delay: Duration, gate: Option<&AtomicBool>) -> bool {
    let start = Instant::now();
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return false;
        }
        let open = gate.is_none_or(|gate| gate.load(Ordering::Acquire));
        let waited = start.elapsed();
        if open && waited >= delay {
            return true;
        }
        let step = if open { delay - waited } else { Duration::from_millis(2) };
        std::thread::sleep(step.min(Duration::from_millis(5)));
    }
}

/// The next response for `request`: the first keyed script whose key the
/// body contains and that has a response left, else the unkeyed script, else
/// a 500 that says nothing was scripted.
fn next_response(shared: &Shared, request: &Received) -> Scripted {
    let mut keyed = shared.keyed.lock().unwrap_or_else(PoisonError::into_inner);
    let from_key = keyed
        .iter_mut()
        .find(|(key, queue)| !queue.is_empty() && request.body.contains(key.as_str()))
        .and_then(|(_, queue)| queue.pop_front());
    drop(keyed);
    from_key
        .map(|reply| reply(request))
        .or_else(|| shared.script.lock().unwrap_or_else(PoisonError::into_inner).pop_front())
        .unwrap_or(Scripted {
            status: 500,
            headers: Vec::new(),
            body: "{\"error\":{\"message\":\"unscripted\"}}".into(),
            delay: Duration::ZERO,
            events: None,
            hold: None,
        })
}

pub struct FakeHttp {
    shared: Arc<Shared>,
    port: u16,
    thread: Option<JoinHandle<()>>,
}

fn read_request(stream: &TcpStream) -> Option<Received> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let mut headers = BTreeMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Received { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

fn serve(shared: &Shared, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Some(request) = read_request(&stream) else { return };
    let scripted = next_response(shared, &request);
    shared.received.lock().unwrap_or_else(PoisonError::into_inner).push(request);
    if !wait(shared, scripted.delay, scripted.hold.as_deref()) {
        return;
    }
    if let Some((events, gap)) = &scripted.events {
        let head = format!(
            "HTTP/1.1 {} X\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            scripted.status
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        for event in events {
            // A keep-alive comment first, as OpenRouter sends them.
            if stream
                .write_all(format!(": keep-alive\n\ndata: {event}\n\n").as_bytes())
                .and_then(|()| stream.flush())
                .is_err()
            {
                return; // the adapter hung up: the stream was cancelled or abandoned
            }
            let gate = shared.stream_gates.lock().unwrap_or_else(PoisonError::into_inner).remove(event);
            if !wait(shared, *gap, gate.as_deref()) {
                return;
            }
        }
        return;
    }
    let mut response = format!(
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        scripted.status,
        scripted.body.len()
    );
    for (name, value) in &scripted.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    response.push_str(&scripted.body);
    let _ = stream.write_all(response.as_bytes());
}

impl FakeHttp {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let shared = Arc::new(Shared::default());
        let serving = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if serving.stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(stream) = stream {
                    // One thread per connection: the adapter has several calls in flight.
                    // Not joined: once the server stops, each ends at its next wait or write.
                    let shared = Arc::clone(&serving);
                    std::thread::spawn(move || serve(&shared, stream));
                }
            }
        });
        Self { shared, port, thread: Some(thread) }
    }

    /// Base URL to give the adapter, including an API prefix.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/api/v1", self.port)
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn script(&self, response: Scripted) {
        self.shared.script.lock().unwrap_or_else(PoisonError::into_inner).push_back(response);
    }

    /// Pause after the first SSE event until the test observes its effects.
    /// This keeps finalization from racing the consumer's first stream pull.
    pub fn script_gated_stream(&self, response: Scripted) -> StreamGate {
        let event = response
            .events
            .as_ref()
            .and_then(|(events, _)| events.first())
            .expect("a gated stream needs at least one event")
            .clone();
        let released = Arc::new(AtomicBool::new(false));
        let previous = self
            .shared
            .stream_gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(event, Arc::clone(&released));
        assert!(previous.is_none(), "the same event already has a stream gate");
        self.script(response);
        StreamGate(released)
    }

    /// Scripts `response` for the requests whose body contains `key`, in
    /// order per key. Lets one server answer several concurrent conversations
    /// deterministically — e.g. the workers of two problems whose prompts
    /// name different theorems — while requests within one key still race for
    /// its queue. A request matching no key with responses left falls back to
    /// the unkeyed [`Self::script`].
    pub fn script_for(&self, key: &str, response: Scripted) {
        self.script_with(key, move |_| response);
    }

    /// Computes one response from the actual request, e.g. its generated workspace path.
    pub fn script_with(&self, key: &str, response: impl FnOnce(&Received) -> Scripted + Send + 'static) {
        let response: ResponseFactory = Box::new(response);
        let mut keyed = self.shared.keyed.lock().unwrap_or_else(PoisonError::into_inner);
        match keyed.iter_mut().find(|(k, _)| k == key) {
            Some((_, queue)) => queue.push_back(response),
            None => keyed.push((key.to_owned(), VecDeque::from([response]))),
        }
    }

    pub fn received(&self) -> Vec<Received> {
        self.shared.received.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so it can see the flag.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A well-formed chat completion as OpenRouter returns it.
pub fn completion(model: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "gen-test",
        "model": model,
        "provider": "DeepSeek",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": content, "reasoning": null}}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13, "cost": 0.000_012}
    })
}

/// A chat completion in which the model asks for tools. `calls` are
/// `(id, tool name, arguments)`.
pub fn tool_request(model: &str, calls: &[(&str, &str, serde_json::Value)]) -> serde_json::Value {
    let tool_calls: Vec<serde_json::Value> = calls
        .iter()
        .map(|(id, name, arguments)| {
            serde_json::json!({"id": id, "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}})
        })
        .collect();
    serde_json::json!({
        "id": "gen-test", "model": model,
        "choices": [{"index": 0, "finish_reason": "tool_calls",
                     "message": {"role": "assistant", "content": null, "tool_calls": tool_calls}}],
        "usage": {"prompt_tokens": 20, "completion_tokens": 8}
    })
}
