#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::sync::atomic::AtomicBool;
use std::sync::mpsc as std_mpsc;

use rustls::pki_types::PrivateKeyDer;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::*;
use crate::serve::Job;

/// What a fake platform signs keys with, as the platform does with its own.
struct Ca {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    root: rustls::pki_types::CertificateDer<'static>,
}
impl Ca {
    fn new() -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
        let root = params.self_signed(&key).unwrap();
        Self { root: root.der().clone(), issuer: rcgen::Issuer::new(params, key) }
    }
    /// A new key it signed: its certificate and private key, PEM.
    fn key(&self) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        params.subject_alt_names =
            vec![rcgen::SanType::URI("nucleus-vm:o-test/avm-test".try_into().unwrap())];
        let certificate = params.signed_by(&key, &self.issuer).unwrap();
        (certificate.pem(), key.serialize_pem())
    }
}

fn config(platform: &Platform) -> LinkConfig {
    LinkConfig::parse(&file(platform, &platform.ca.key())).unwrap()
}

/// An inbox whose jobs a thread answers with `answer(job)`.
fn inbox(answer: impl Fn(&Job) -> Value + Send + 'static) -> Inbox {
    let (jobs, requests) = std_mpsc::channel::<Job>();
    std::thread::spawn(move || {
        for job in requests {
            drop(job.answer.send(respond(Some(job.id), Ok(answer(&job)))));
        }
    });
    Inbox::new(jobs, Arc::new(|| true))
}

fn label() -> String {
    json!({"v": 1, "kind": "window", "account": "a-1", "nickname": "Ada", "role": "member"}).to_string()
}

/// A fake platform: QUIC with a self-signed certificate and the link's ALPN,
/// taking only clients with a key its [`Ca`] signed; it reads each
/// connection's hello and closes the connection with `refuse` when that names
/// a status; otherwise it hands over the hello (with a broker's request), the
/// certificate the client showed, the connection and the hello's stream,
/// saying nothing.
struct Platform {
    address: SocketAddr,
    pin: [u8; 32],
    ca: Ca,
    connections: mpsc::UnboundedReceiver<(Value, Vec<u8>, Connection, SendStream)>,
}

async fn platform(refuse: Option<u32>) -> Platform {
    let certified = rcgen::generate_simple_self_signed(vec!["platform".to_owned()]).unwrap();
    let certificate = certified.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
    let pin = pin_of(certificate.as_ref()).unwrap();
    let ca = Ca::new();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.root.clone()).unwrap();
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .unwrap();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![certificate], key)
        .unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let endpoint = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(crypto)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let address = endpoint.local_addr().unwrap();
    let (arrived, connections) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let arrived = arrived.clone();
            tokio::spawn(async move {
                let Ok(connection) = incoming.await else { return };
                let shown = connection
                    .peer_identity()
                    .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
                    .and_then(|chain| chain.first().map(|leaf| leaf.as_ref().to_vec()))
                    .unwrap_or_default();
                let Ok((send, recv)) = connection.accept_bi().await else { return };
                let mut reader = Reader { recv, pending: Vec::new() };
                let hello = reader.line(MAX_REQUEST_LINE).await.unwrap();
                if let Some(status) = refuse {
                    connection.close(VarInt::from_u32(status), b"refused");
                    return;
                }
                let mut hello: Value = serde_json::from_slice(&hello).unwrap();
                // A broker's request is the line after its hello, read once that is taken.
                if hello["program"] == "broker" {
                    let request = reader.line(MAX_REQUEST_LINE).await.unwrap();
                    hello["request"] = serde_json::from_slice(&request).unwrap();
                }
                drop(arrived.send((hello, shown, connection, send)));
            });
        }
    });
    Platform { address, pin, ca, connections }
}

async fn next<T>(from: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(Duration::from_secs(5), from.recv()).await.expect("in time").unwrap()
}
/// A stream the platform opens on `connection` with `label`, and its reader.
async fn window(connection: &Connection, label: &str) -> (SendStream, Reader) {
    let (mut send, recv) = connection.open_bi().await.unwrap();
    send.write_all(format!("{label}\n").as_bytes()).await.unwrap();
    (send, Reader { recv, pending: Vec::new() })
}
async fn line(reader: &mut Reader) -> Value {
    let line = timeout(Duration::from_secs(5), reader.line(8 << 20)).await.unwrap().unwrap();
    serde_json::from_slice(&line).unwrap()
}
/// The link file for `platform` with key `key`.
fn file(platform: &Platform, key: &(String, String)) -> String {
    let pin = base64::engine::general_purpose::STANDARD.encode(platform.pin);
    json!({"address": platform.address.to_string(), "pin": format!("sha256/{pin}"),
           "certificate": key.0, "key": key.1})
    .to_string()
}
/// Puts `content` at `path` as the host does: written beside, renamed into place.
fn put(path: &Path, content: &str) {
    let beside = path.with_extension("json.new");
    std::fs::write(&beside, content).unwrap();
    std::fs::rename(&beside, path).unwrap();
}
/// The DER of the certificate in `key`.
fn der(key: &(String, String)) -> Vec<u8> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::from_pem_slice(key.0.as_bytes()).unwrap().as_ref().to_vec()
}

#[test]
fn the_link_s_configuration_is_exactly_its_fields() {
    let pin = format!("sha256/{}", base64::engine::general_purpose::STANDARD.encode([7u8; 32]));
    let (certificate, key) = Ca::new().key();
    let file =
        |address: &str| json!({"address": address, "pin": pin, "certificate": certificate, "key": key});
    for address in ["100.64.0.7:8443", "[fd7a:115c::1]:8443"] {
        let config =
            LinkConfig::parse(&file(address).to_string()).unwrap_or_else(|e| panic!("{address}: {e}"));
        assert_eq!((config.address, config.pin), (address.parse().unwrap(), [7u8; 32]));
    }
    let good = file("10.0.0.1:1");
    for (field, bad) in [
        ("address", json!("platform.example:1")),
        ("address", json!("10.0.0.1")),
        ("pin", json!("sha1/AAAA")),
        ("pin", json!(format!("sha256/{}", base64::engine::general_purpose::STANDARD.encode([7u8; 31])))),
        ("extra", json!(true)),
    ] {
        let mut file = good.clone();
        file[field] = bad.clone();
        assert!(LinkConfig::parse(&file.to_string()).is_err(), "{field} {bad}");
    }
    let mut missing = good;
    missing.as_object_mut().unwrap().remove("key");
    assert!(LinkConfig::parse(&missing.to_string()).is_err());
}

/// The pin is the SHA-256 of the certificate's SubjectPublicKeyInfo, as the
/// platform computes it from its key.
#[test]
fn a_pin_is_the_hash_of_the_certificate_s_key() {
    use rcgen::PublicKeyData;
    for _ in 0..3 {
        let certified = rcgen::generate_simple_self_signed(vec!["platform".to_owned()]).unwrap();
        let spki = certified.signing_key.subject_public_key_info();
        assert_eq!(pin_of(certified.cert.der()), Some(sha2::Sha256::digest(&spki).into()));
    }
}

/// A connection goes to the pinned key only, shows the key the file holds,
/// says its hello, and serves a window on each stream the platform opens; a
/// refusal is the close code. A new key put in the file is connected with at
/// once, while the connection before keeps serving its windows.
#[tokio::test]
async fn a_connection_shows_its_key_and_a_new_key_connects_again_while_the_windows_go_on() {
    let mut other = platform(None).await;
    let mut pinned = platform(None).await;
    let mut elsewhere = config(&pinned);
    elsewhere.address = other.address;
    assert!(connect(&elsewhere, None, &AtomicBool::new(false), "").await.is_err());
    assert!(other.connections.try_recv().is_err(), "no hello reaches another key");
    let refusing = platform(Some(401)).await;
    let (refused, _) = connect(&config(&refusing), None, &AtomicBool::new(false), "").await.unwrap();
    let why = ended(timeout(Duration::from_secs(5), refused.closed()).await.unwrap());
    assert!(why.contains("(401)"), "{why}");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("platform.json");
    let first_key = pinned.ca.key();
    put(&path, &file(&pinned, &first_key));
    run(
        path.clone(),
        inbox(|job| match &job.operation {
            crate::Operation::RunsList { project: Some(project), .. } => json!(project),
            _ => json!({}),
        }),
    );
    let (hello, shown, connection, _) = next(&mut pinned.connections).await;
    let process = hello["process"].clone();
    assert_eq!(process.as_str().map(str::len), Some(32));
    assert_eq!(hello, json!({"program": "ui", "process": process}));
    assert_eq!(shown, der(&first_key));
    let (mut first, mut first_lines) = window(&connection, &label()).await;
    let (mut second, mut second_lines) = window(&connection, &label()).await;
    for lines in [&mut first_lines, &mut second_lines] {
        assert_eq!(line(lines).await, json!({"ok": true}));
    }
    first.write_all(b"{\"id\":1,\"op\":\"runs.list\",\"project\":\"alpha\"}\n").await.unwrap();
    second.write_all(b"{\"id\":1,\"op\":\"runs.list\",\"project\":\"beta\"}\n").await.unwrap();
    assert_eq!(line(&mut second_lines).await, json!({"id": 1, "ok": "beta"}));
    assert_eq!(line(&mut first_lines).await, json!({"id": 1, "ok": "alpha"}));
    let (_refused, mut refused) = window(&connection, r#"{"member":"Ada"}"#).await;
    assert_eq!(line(&mut refused).await["error"]["code"], "invalid_input");
    // A new key: a new connection of the same process shows it at once; the window
    // before goes on.
    let next_key = pinned.ca.key();
    put(&path, &file(&pinned, &next_key));
    let (hello, shown, newer, _) = next(&mut pinned.connections).await;
    assert_eq!((shown, &hello["process"]), (der(&next_key), &process));
    first.write_all(b"{\"id\":2,\"op\":\"runs.list\",\"project\":\"gamma\"}\n").await.unwrap();
    assert_eq!(line(&mut first_lines).await, json!({"id": 2, "ok": "gamma"}));
    // The newer one closed by the platform: the next connection shows the file's key.
    newer.close(VarInt::from_u32(3), b"not the team's VM now");
    let (_, shown, _, _) = next(&mut pinned.connections).await;
    assert_eq!(shown, der(&next_key));
}

/// The broker says each request after the hello of a connection of its own,
/// showing the key the file holds then, and reads the answer on the hello's
/// stream; one refused (by a status, or its key in the handshake) was not acted on. The next key is
/// asked for the same way, and nothing answered.
#[tokio::test]
async fn the_broker_says_each_request_after_the_hello_of_a_connection_of_its_own() {
    let mut platform = platform(None).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("platform.json");
    let key = platform.ca.key();
    put(&path, &file(&platform, &key));
    let calling = tokio::spawn(crate::github::call(path.clone(), json!({"op":"status","project":"alpha"})));
    let (hello, shown, connection, mut send) = next(&mut platform.connections).await;
    assert_eq!((&hello["program"], shown), (&json!("broker"), der(&key)));
    assert_eq!(hello["request"], json!({"route": "github", "body": {"op":"status","project":"alpha"}}));
    let big = "x".repeat(256 * 1024);
    let answer = json!({"ok": {"repository": "Lab/private", "default_branch": "trunk", "body": big}});
    send.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
    send.finish().unwrap();
    drop(timeout(Duration::from_secs(5), connection.closed()).await.unwrap());
    let answered = calling.await.unwrap().unwrap().decode::<Value>().unwrap();
    assert_eq!(answered["body"], big);
    let asking = tokio::spawn({
        let path = path.clone();
        async move { ask_next_key(&path).await }
    });
    let (hello, _, connection, _) = next(&mut platform.connections).await;
    assert_eq!(hello["request"], json!({"route": "key", "body": {}}));
    connection.close(VarInt::from_u32(0), b"");
    timeout(Duration::from_secs(5), asking).await.unwrap().unwrap().unwrap();
    let refusing = self::platform(Some(401)).await;
    put(&path, &file(&refusing, &refusing.ca.key()));
    assert_eq!(
        request(&path, "request", &json!({"op": "list"}), &AtomicBool::new(false)).await,
        Err(Lost::Unsent)
    );
    // A key it did not sign is refused in the handshake: not sent either.
    put(&path, &file(&platform, &Ca::new().key()));
    assert_eq!(
        request(&path, "request", &json!({"op": "list"}), &AtomicBool::new(false)).await,
        Err(Lost::Unsent)
    );
}

#[test]
fn the_jitter_is_random_and_under_a_second() {
    let drawn: Vec<Duration> = (0..200).map(|_| jitter()).collect();
    assert!(drawn.iter().all(|j| *j < JITTER));
    assert!(drawn.iter().any(|j| *j != drawn[0]), "not one value");
}
