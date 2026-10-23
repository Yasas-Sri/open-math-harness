//! The link: this service is a client of the platform, its key the one the
//! platform wrote into the VM. It keeps a connection to the platform, which
//! opens a stream on it for each window.
//!
//! **Config.** `--link <file>` names the link's configuration, which the
//! platform writes into the VM over its management channel, never over the
//! link (`/var/lib/nucleus/secrets/link/platform.json`, 0600 `nucleus`, its
//! directory 0700). The directory is bound read-only into two sandboxes,
//! this service's and the GitHub broker's (`github`, the same executable):
//!
//! ```text
//! {"address":"IP:PORT","pin":"sha256/<base64 SHA-256 of the link certificate's SPKI>",
//!  "certificate":"<PEM: this VM's key's certificate, the platform's>","key":"<PEM: its private key>"}
//! ```
//!
//! The address is the platform's link (`[…]:PORT` for IPv6); no name is
//! looked up. The file is read again for every connection and every request
//! of the broker; one that is missing or not one is reported and read again
//! for the next. A process of the runtime's user outside a sandbox, or one
//! that escapes its sandbox, reads the key as it reads the model key; with
//! it, it can be this VM to the platform until the key is no longer good, and
//! never get the next one, which comes into the VM through its host.
//!
//! **Connection.** QUIC over UDP to the address, TLS 1.3 only and ALPN [`ALPN`],
//! accepting the server only if the SHA-256 of its leaf certificate's
//! SubjectPublicKeyInfo is `pin`: no CA, no name, no validity dates; the
//! handshake's signature is still checked against that key. This side shows
//! its key in the handshake, which the platform checks: a key it did not sign
//! fails the handshake. On the connection this side opens one stream and says
//! its hello, one line: `{"program":"ui","process"}`, `process` a random name
//! this service chose when it started (the platform closes a connection of a
//! process before it at once, and lets one of this process's own drain); who
//! it is the key says. The platform answers nothing; one that refuses closes
//! the connection, the status as its close code (401: a key no longer good, or
//! not the team's VM now), which is reported, and the connection opened again.
//!
//! **Streams.** The platform opens a stream for each window: its first line is
//! the window's label (at most 4 KiB, module `access`), which this service
//! answers with `{"ok":true}`, or refuses with a line saying why. From then on
//! the stream is that window's connection and carries the UI line protocol, a
//! byte stream of JSON lines as on a socket (module `window`). Either side
//! finishing its side ends that window only. The broker opens a connection of
//! its own for each request it makes ([`request`]): its hello says
//! `"program":"broker"`, the next line is the request, `{"route","body"}`,
//! which the platform checks as it reads it, and the answer is one line on the
//! same stream.
//!
//! **Keys.** A key is good for as long as its certificate says, a day. For as long
//! as this service runs, past half of that in its file's age (on this VM's own
//! clock) it asks for the
//! next one, route `key`, and again every [`RENEW_AGAIN`] until it comes: the
//! platform answers nothing and writes the next into the VM through its host.
//! A key that ran out first reaches nothing; the platform writes a new one when
//! its team is next opened. When the file holds
//! another key (the host renames a new one into place) this service connects
//! with it at once; the connection before keeps serving its windows until the
//! platform closes it, once they end.
//!
//! **Liveness.** A keepalive every [`KEEPALIVE`]; a connection silent for
//! [`IDLE`] ends, as one the platform resets does (a restarted platform resets
//! the connections it does not hold). The connection is then opened again
//! after 2 s plus 0–1 s of random jitter, or at once when the file changes,
//! for as long as the service runs. One whose key was refused (a key no longer
//! good, not the team's VM now, a pin no longer the platform's) is opened again
//! only when the file changes: nothing else would change the answer. A VM moved
//! by a live migration keeps its connection: QUIC follows the new address.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use quinn::{Connection, RecvStream, SendStream, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest;
use tokio::sync::Notify;
use tokio::time::sleep;

use crate::Peer;
use crate::serve::{Inbox, respond};
use crate::window::{MAX_ANSWER_BYTES, MAX_REQUEST_BYTES, Window, encode};
use nucleus_protocol::{Error, ErrorCode};

/// The link's QUIC application protocol.
pub const ALPN: &[u8] = b"nucleus-vm-link-4";
/// How often the next key is asked for, once due, until it comes.
pub const RENEW_AGAIN: Duration = Duration::from_secs(3600);
/// After a connection ends or fails, the next is opened this much later, plus jitter.
pub const REDIAL: Duration = Duration::from_secs(2);
/// The most jitter added to [`REDIAL`].
pub const JITTER: Duration = Duration::from_secs(1);
/// How often this side sends a keepalive.
pub const KEEPALIVE: Duration = Duration::from_secs(10);
/// A connection silent this long ends.
pub const IDLE: Duration = Duration::from_secs(30);
/// Windows the platform may have open at once on this service's connection.
pub const MAX_STREAMS: u32 = 256;
/// Longest label line.
const MAX_LABEL_BYTES: usize = 4096;
/// The longest broker request line the platform takes.
pub(crate) const MAX_REQUEST_LINE: usize = 64 * 1024;
/// Flow control: what one stream may have unread; and what this side sends before it is
/// acknowledged.
const STREAM_WINDOW: u32 = 1 << 20;
const SEND_WINDOW: u64 = 16 << 20;

/// The link's configuration (module doc), as read and checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkConfig {
    pub address: SocketAddr,
    pub pin: [u8; 32],
    /// The key's certificate and private key, PEM.
    pub certificate: String,
    pub key: String,
}

/// The file as the platform writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    address: SocketAddr,
    pin: String,
    certificate: String,
    key: String,
}

impl LinkConfig {
    /// Reads and checks the configuration file.
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|why| format!("{}: {why}", path.display()))
    }

    /// Checks a configuration (module doc).
    pub fn parse(text: &str) -> Result<Self, String> {
        let File { address, pin, certificate, key } =
            serde_json::from_str(text).map_err(|e| format!("not the link's configuration: {e}"))?;
        let pin = pin
            .strip_prefix("sha256/")
            .and_then(|pin| base64::engine::general_purpose::STANDARD.decode(pin).ok())
            .and_then(|pin| <[u8; 32]>::try_from(pin).ok())
            .ok_or("the pin is not `sha256/` and the base64 of 32 bytes")?;
        Ok(Self { address, pin, certificate, key })
    }

    /// Half its key's life, as its certificate says: when its next key is due.
    fn half_life(&self) -> Option<Duration> {
        let der = CertificateDer::from_pem_slice(self.certificate.as_bytes()).ok()?;
        let (_, certificate) = x509_parser::parse_x509_certificate(der.as_ref()).ok()?;
        let validity = certificate.validity();
        let life = validity.not_after.timestamp() - validity.not_before.timestamp();
        Some(Duration::from_secs(u64::try_from(life / 2).ok()?))
    }

    /// The key as TLS shows it: its certificate chain and its private key.
    fn identity(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
        let chain = CertificateDer::pem_slice_iter(self.certificate.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .ok()
            .filter(|chain| !chain.is_empty())
            .ok_or("the key's certificate is not PEM")?;
        let key =
            PrivateKeyDer::from_pem_slice(self.key.as_bytes()).map_err(|_| "the private key is not PEM")?;
        Ok((chain, key))
    }
}

/// The SHA-256 of a certificate's SubjectPublicKeyInfo.
pub(crate) fn pin_of(certificate: &[u8]) -> Option<[u8; 32]> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate).ok()?;
    Some(sha2::Sha256::digest(certificate.public_key().raw).into())
}

/// Accepts exactly the server whose leaf key is pinned (module doc).
#[derive(Debug)]
struct Pinned {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match pin_of(end_entity.as_ref()) {
            Some(pin) if pin == self.pin => Ok(ServerCertVerified::assertion()),
            _ => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// TLS 1.3 to the server whose key is `config`'s pin, and to nobody else,
/// showing `config`'s key.
pub(crate) fn tls_config(config: &LinkConfig) -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (chain, key) = config.identity()?;
    rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS 1.3: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { pin: config.pin, provider }))
        .with_client_auth_cert(chain, key)
        .map_err(|e| format!("the link's key: {e}"))
}

/// Reads a stream's lines; what was read past the last one stays in `pending`.
struct Reader {
    recv: RecvStream,
    pending: Vec<u8>,
}
impl Reader {
    /// The next line, without its newline, of at most `max` bytes; none when the stream
    /// ends first, is lost, or the line is longer.
    async fn line(&mut self, max: usize) -> Option<Vec<u8>> {
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            if let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                if end > max {
                    return None;
                }
                let mut line: Vec<u8> = self.pending.drain(..=end).collect();
                line.pop();
                return Some(line);
            }
            if self.pending.len() > max {
                return None;
            }
            let n = self.recv.read(&mut chunk).await.ok()??;
            self.pending.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Why a connection could not be had, as the log says it, and whether its key was
/// refused ([`refused`]): then only another link file changes anything.
#[derive(Debug)]
pub(crate) struct Failed {
    why: String,
    refused: bool,
}
impl From<String> for Failed {
    fn from(why: String) -> Self {
        Self { why, refused: false }
    }
}

/// Whether `error` refused this side's link file: the platform's TLS alert (a key it did
/// not sign, or one no longer good), the platform's 401 (a key no longer good, or not the
/// team's VM now), or this side's own (a pin no longer the platform's).
fn refused(error: &quinn::ConnectionError) -> bool {
    match error {
        quinn::ConnectionError::ApplicationClosed(close) => close.error_code.into_inner() == 401,
        quinn::ConnectionError::ConnectionClosed(close) => u64::from(close.error_code) >> 8 == 1,
        quinn::ConnectionError::TransportError(error) => u64::from(error.code) >> 8 == 1,
        _ => false,
    }
}

/// Opens a connection as `config` says: QUIC to the pinned key, then the hello,
/// and after a broker's its `request` (module doc), `saying` set as that starts
/// to go. The hello's stream, which a broker's answer comes back on. A
/// connection that fails never said its whole request: the platform did nothing
/// for it. A
/// platform that cannot be reached answers nothing (UDP): QUIC gives up at
/// [`IDLE`], or the caller does first.
async fn connect(
    config: &LinkConfig,
    request: Option<Value>,
    saying: &AtomicBool,
    process: &str,
) -> Result<(Connection, RecvStream), Failed> {
    let address = config.address;
    let mut tls = tls_config(config)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| format!("QUIC TLS: {e}"))?;
    let mut client = quinn::ClientConfig::new(Arc::new(crypto));
    let windows = if request.is_some() { 0 } else { MAX_STREAMS };
    let mut transport = TransportConfig::default();
    transport
        .keep_alive_interval(Some(KEEPALIVE))
        .max_idle_timeout(IDLE.try_into().ok())
        .max_concurrent_bidi_streams(VarInt::from_u32(windows))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .datagram_receive_buffer_size(None)
        .stream_receive_window(VarInt::from_u32(STREAM_WINDOW))
        // As much as all its streams together: a window read slowly holds up no other.
        .receive_window(VarInt::from_u32(MAX_STREAMS * STREAM_WINDOW))
        .send_window(SEND_WINDOW);
    client.transport_config(Arc::new(transport));
    let local: SocketAddr = match address {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let mut endpoint = quinn::Endpoint::client(local).map_err(|e| format!("a UDP socket: {e}"))?;
    endpoint.set_default_client_config(client);
    let program = if request.is_some() { "broker" } else { "ui" };
    let hello = json!({"program": program, "process": process});
    let connection = endpoint
        .connect(address, "link")
        .map_err(|e| format!("QUIC to {address}: {e}"))?
        .await
        .map_err(|e| Failed { why: format!("QUIC with {address}: {e}"), refused: refused(&e) })?;
    // Lost as it began: refused, or not, as the connection's end says.
    let lost = |what: &str| Failed {
        why: format!("{what}: {}", connection.close_reason().map_or("lost".to_owned(), ended)),
        refused: connection.close_reason().is_some_and(|reason| refused(&reason)),
    };
    let (mut send, recv) = connection.open_bi().await.map_err(|_| lost("the hello's stream"))?;
    let mut line = hello.to_string().into_bytes();
    line.push(b'\n');
    send.write_all(&line).await.map_err(|_| lost("the hello"))?;
    // A broker's request follows its hello, taken once the hello is admitted.
    if let Some(request) = request {
        let mut line = request.to_string().into_bytes();
        line.push(b'\n');
        saying.store(true, Ordering::SeqCst);
        send.write_all(&line).await.map_err(|_| lost("the request"))?;
    }
    drop(send.finish());
    Ok((connection, recv))
}

/// Why a connection ended, as the log says it.
fn ended(error: quinn::ConnectionError) -> String {
    match error {
        quinn::ConnectionError::TimedOut => format!("the connection was silent for {IDLE:?}"),
        quinn::ConnectionError::Reset => "the platform no longer holds the connection".to_owned(),
        quinn::ConnectionError::ApplicationClosed(close) => match close.error_code.into_inner() {
            2 => "a newer connection replaced this one".to_owned(),
            3 => "this VM is not the team's VM now".to_owned(),
            5 => "a connection with a newer key replaced this one".to_owned(),
            401 => "the platform refused the link: a key no longer good, or not the team's VM now (401)"
                .to_owned(),
            status => format!("the platform closed the connection ({status})"),
        },
        error => format!("the connection ended: {error}"),
    }
}

/// Keeps a connection with the platform the configuration at `config` names,
/// for as long as the service runs (module doc): each stream the platform
/// opens is a window's, served by a task of its own; a connection that ends or
/// fails is opened again after [`REDIAL`] and the jitter, or at once when the
/// file changes, while connecting too. One whose key was refused ([`refused`])
/// is opened again only when the file changes. A file that changes while connected (a
/// new key) is connected with at once; the connection before serves its windows
/// until the platform ends it. The next key is asked for as [`renew`] says.
pub(crate) fn run(config: PathBuf, inbox: Inbox) {
    tokio::spawn(async move {
        let config = Arc::new(config);
        // This process's name, in each hello: the platform tells a connection of this
        // process's from one of a process before it.
        let process = format!("{:016x}{:016x}", random(), random());
        let changed = Arc::new(Notify::new());
        if let Err(why) = watch(&config, changed.clone()) {
            eprintln!("nucleus-ui: link: {why}; this service sees no new key until it starts again");
        }
        tokio::spawn(renew(config.clone()));
        // A failure reported and not yet followed by a window: said once, and again only
        // when the reason changes; the first window after it says the link is back.
        let reported = Arc::new(AtomicBool::new(false));
        let mut failing: Option<String> = None;
        // A UI connection has no request, so nothing is said past its hello.
        let unsaid = AtomicBool::new(false);
        loop {
            let Failed { why, refused: refused_key } = match LinkConfig::read(&config) {
                Err(why) => why.into(),
                Ok(read) => match tokio::select! {
                    connected = connect(&read, None, &unsaid, &process) => connected,
                    // A new file meanwhile: the attempt goes, and the file is read again.
                    () = changed.notified() => continue,
                } {
                    Err(failed) => failed,
                    Ok((connection, _)) => {
                        tokio::spawn(serve(connection.clone(), inbox.clone(), reported.clone()));
                        // Connected with this key until the connection ends or the file holds
                        // another: a change that leaves the key as it was is none.
                        let ended = loop {
                            tokio::select! {
                                () = changed.notified() => {
                                    let now = LinkConfig::read(&config);
                                    if now.is_ok_and(|now| now.certificate != read.certificate) {
                                        break None;
                                    }
                                }
                                error = connection.closed() => {
                                    break Some(Failed { refused: refused(&error), why: ended(error) });
                                }
                            }
                        };
                        match ended {
                            Some(failed) => failed,
                            None => continue,
                        }
                    }
                },
            };
            if !reported.load(Ordering::SeqCst) {
                failing = None;
            }
            if failing.as_deref() != Some(why.as_str()) {
                let again = if refused_key {
                    "connecting again once the platform writes this VM another link file".to_owned()
                } else {
                    format!("connecting again every {REDIAL:?} and a jitter")
                };
                eprintln!("nucleus-ui: link: {why}; {again}");
                failing = Some(why);
                reported.store(true, Ordering::SeqCst);
            }
            // A key refused is refused again until another file comes; anything else may
            // pass on its own.
            if refused_key {
                changed.notified().await;
            } else {
                tokio::select! {
                    () = sleep(REDIAL + jitter()) => {}
                    () = changed.notified() => {}
                }
            }
        }
    });
}

/// Serves the windows the platform opens on `connection` until it ends; the first after
/// a failure was `reported` says the link is back.
async fn serve(connection: Connection, inbox: Inbox, reported: Arc<AtomicBool>) {
    while let Ok((send, recv)) = connection.accept_bi().await {
        if reported.swap(false, Ordering::SeqCst) {
            eprintln!("nucleus-ui: link: connected to the platform again");
        }
        tokio::spawn(stream(send, recv, inbox.clone()));
    }
}

/// Wakes `changed` each time the file at `config` is put anew: the host
/// writes it beside and renames it into place.
fn watch(config: &Path, changed: Arc<Notify>) -> Result<(), String> {
    use futures_util::StreamExt;
    let (Some(dir), Some(name)) = (config.parent(), config.file_name()) else {
        return Err(format!("{} names no file in a directory", config.display()));
    };
    let name = name.to_owned();
    let inotify = inotify::Inotify::init().map_err(|e| format!("inotify: {e}"))?;
    inotify
        .watches()
        .add(dir, inotify::WatchMask::MOVED_TO)
        .map_err(|e| format!("watching {}: {e}", dir.display()))?;
    let mut events = inotify.into_event_stream(vec![0u8; 4096]).map_err(|e| format!("inotify: {e}"))?;
    let watched = config.display().to_string();
    tokio::spawn(async move {
        while let Some(Ok(event)) = events.next().await {
            if event.name.as_deref() == Some(name.as_os_str()) {
                changed.notify_one();
            }
        }
        eprintln!(
            "nucleus-ui: link: watching {watched} ended; this service sees no new key until it starts again"
        );
    });
    Ok(())
}

/// Asks for the next key while the file at `config` is past half its key's life
/// (its age on this VM's own clock, so that no other clock counts; the life its
/// certificate's), every [`RENEW_AGAIN`], for as long as the service runs: the
/// next key, put in the file, makes it young again.
async fn renew(config: Arc<PathBuf>) {
    loop {
        // What is unknown (no file to read, or one written in this clock's future) is
        // due: the platform answers a younger key's asking with nothing.
        let due = LinkConfig::read(&config).ok().and_then(|file| file.half_life()).unwrap_or_default();
        let age = std::fs::metadata(&*config)
            .and_then(|file| file.modified())
            .ok()
            .and_then(|written| written.elapsed().ok())
            .unwrap_or(due);
        if age < due {
            sleep((due - age).min(RENEW_AGAIN)).await;
            continue;
        }
        if let Err(why) = ask_next_key(&config).await {
            eprintln!("nucleus-ui: link: the next key could not be asked for: {why}");
        }
        sleep(RENEW_AGAIN).await;
    }
}

/// Asks the platform for this VM's next key (route `key`): it answers nothing,
/// writes the key through the VM's host, and closes the connection.
async fn ask_next_key(path: &Path) -> Result<(), String> {
    let config = LinkConfig::read(path)?;
    let asking = json!({"route": "key", "body": {}});
    let (connection, _) =
        connect(&config, Some(asking), &AtomicBool::new(false), "").await.map_err(|failed| failed.why)?;
    connection.closed().await;
    Ok(())
}

/// Finishes a stream with a line saying why, in the line protocol's error shape.
async fn refuse(send: &mut SendStream, why: &str) {
    let line =
        encode(&respond(None, Err(Error::new(ErrorCode::InvalidInput, why.to_owned()))), MAX_ANSWER_BYTES);
    drop(send.write_all(&line).await);
    drop(send.finish());
}

/// A stream the platform opened: its label is read and answered, then the
/// window served (module doc).
async fn stream(mut send: SendStream, recv: RecvStream, inbox: Inbox) {
    let mut reader = Reader { recv, pending: Vec::new() };
    let Some(label) = reader.line(MAX_LABEL_BYTES).await else {
        return refuse(&mut send, "a window's label is one line of at most 4 KiB").await;
    };
    let peer = match std::str::from_utf8(&label).map_err(|e| e.to_string()).and_then(Peer::from_label) {
        Ok(peer) => peer,
        Err(why) => {
            eprintln!("nucleus-ui: link: the platform's label is not one: {why}");
            return refuse(&mut send, &why).await;
        }
    };
    let mut line = json!({"ok": true}).to_string().into_bytes();
    line.push(b'\n');
    if send.write_all(&line).await.is_err() {
        return;
    }
    serve_window(peer, reader, send, inbox).await;
}

/// Serves the window a stream was given until either side ends it.
async fn serve_window(peer: Peer, reader: Reader, mut send: SendStream, inbox: Inbox) {
    let (mut window, mut answers) = Window::new(peer, inbox);
    let Reader { mut recv, mut pending } = reader;
    let mut bytes = vec![0u8; 64 * 1024];
    // The requests whole in `pending`, in order: false once the window ends.
    let requests = |window: &mut Window, pending: &mut Vec<u8>| -> Result<bool, String> {
        let mut start = 0;
        while let Some(at) = pending[start..].iter().position(|&b| b == b'\n') {
            if at > MAX_REQUEST_BYTES {
                return Err(format!("a request line is at most {MAX_REQUEST_BYTES} bytes"));
            }
            if !window.request(&pending[start..start + at]) {
                return Ok(false);
            }
            start += at + 1;
        }
        pending.drain(..start);
        if pending.len() > MAX_REQUEST_BYTES {
            return Err(format!("a request line is at most {MAX_REQUEST_BYTES} bytes"));
        }
        Ok(true)
    };
    match requests(&mut window, &mut pending) {
        Ok(true) => {}
        Ok(false) => return,
        Err(why) => return refuse(&mut send, &why).await,
    }
    loop {
        tokio::select! {
            read = recv.read(&mut bytes), if window.reading() => match read {
                Ok(Some(n)) => {
                    pending.extend_from_slice(&bytes[..n]);
                    match requests(&mut window, &mut pending) {
                        Ok(true) => {}
                        Ok(false) => return,
                        Err(why) => return refuse(&mut send, &why).await,
                    }
                }
                Ok(None) | Err(_) => break,
            },
            Some(answer) = answers.recv() => {
                // A platform that reads slowly holds the window back.
                if send.write_all(&window.encode(&answer)).await.is_err() {
                    return;
                }
                window.written();
            }
            else => break,
        }
    }
    drop(send.finish());
}

/// How a [`request`] failed before the platform answered.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Lost {
    /// The platform did not take it: it was not reached, or it refused the
    /// connection, which it does before it acts.
    Unsent,
    /// Longer than the platform takes ([`MAX_REQUEST_LINE`]): not sent.
    TooLarge,
    /// The request was sent and the connection ended: the platform may have acted on it.
    Unknown,
}

/// One request of the broker (module doc): `route` (`github` or `request`) and
/// `body`, after the hello of a connection opened for it from the configuration
/// at `path` as it is now and closed after; the platform's answer, its `ok` or
/// its `error`. `saying` is set once the request starts to go: a caller that
/// stops waiting before then knows it was not sent.
pub(crate) async fn request(
    path: &Path,
    route: &str,
    body: &Value,
    saying: &AtomicBool,
) -> Result<Result<Value, Value>, Lost> {
    let config = LinkConfig::read(path).map_err(|_| Lost::Unsent)?;
    let request = json!({"route": route, "body": body});
    if request.to_string().len() >= MAX_REQUEST_LINE {
        return Err(Lost::TooLarge);
    }
    let (connection, recv) = connect(&config, Some(request), saying, "").await.map_err(|_| Lost::Unsent)?;
    let mut reader = Reader { recv, pending: Vec::new() };
    let answered = match reader.line(nucleus_protocol::MAX_FRAME_BYTES).await {
        Some(answer) => match serde_json::from_slice::<Value>(&answer) {
            Ok(mut answer) => Ok(match answer.get_mut("ok") {
                Some(ok) => Ok(ok.take()),
                None => Err(answer["error"].take()),
            }),
            Err(_) => Err(Lost::Unknown),
        },
        // A refusal comes before the platform acts: its key refused in the handshake (a
        // TLS alert), or the connection closed with a status. An answer that ended unread
        // on a connection still open is no refusal; it is closed below.
        None => match connection.close_reason() {
            Some(quinn::ConnectionError::ApplicationClosed(close))
                if close.error_code.into_inner() >= 400 =>
            {
                Err(Lost::Unsent)
            }
            Some(quinn::ConnectionError::ConnectionClosed(close))
                if u64::from(close.error_code) >> 8 == 1 =>
            {
                Err(Lost::Unsent)
            }
            _ => Err(Lost::Unknown),
        },
    };
    connection.close(VarInt::from_u32(0), b"answered");
    answered
}

/// A random number, from the process's own random state.
fn random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

/// Random jitter in `[0, JITTER)`.
pub(crate) fn jitter() -> Duration {
    let millis = u64::try_from(JITTER.as_millis()).unwrap_or(1000).max(1);
    Duration::from_millis(random() % millis)
}

#[cfg(test)]
mod tests;
