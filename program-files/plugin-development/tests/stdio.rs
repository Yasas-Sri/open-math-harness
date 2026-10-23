//! Actual service binary and typed public IPC. Host confinement is exercised
//! by the end-to-end tests; this fixture owns only private pipes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nucleus_plugin_development::ACTION_GUIDE;
use nucleus_protocol::{
    ActionName, DispatchId, EffectClass, EndpointName, FromKernel, Generation, HandleId, HandleInfo, Payload,
    ProcessId, ResourcePath, ResponseBody, ToKernel, read_frame, write_frame,
};
use serde_json::{Value, json};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn binary_publishes_one_guide_and_returns_it_over_ordinary_ipc() {
    let directory = tempfile::tempdir().unwrap();
    let guide = directory.path().join("guide.md");
    std::fs::write(&guide, "A guide: café, €1 and \"quoted\" text.\n").unwrap();
    read_from_binary(&guide, "A guide: café, €1 and \"quoted\" text.\n");
}

/// A guide that cannot be served stops the service before it publishes anything.
#[test]
fn a_guide_that_cannot_be_read_fails_before_publishing() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nucleus-plugin-development"))
        .arg("--guide")
        .arg(directory.path().join("missing.md"))
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "must not publish an interface");
}

fn read_from_binary(guide: &Path, expected: &str) {
    let endpoint = EndpointName::new("guide-fixture").unwrap();
    let mut child = Owned(
        Command::new(env!("CARGO_BIN_EXE_nucleus-plugin-development"))
            .args(["--endpoint", endpoint.as_str()])
            .arg("--guide")
            .arg(guide)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = child.0.stdout.take().unwrap();
    let (sender, responses) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        loop {
            let message = read_frame::<_, ToKernel>(&mut output);
            let ended = message.is_err();
            if sender.send(message).is_err() || ended {
                break;
            }
        }
    });
    let receive = || responses.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    write_frame(
        &mut input,
        &FromKernel::Hello {
            process: ProcessId(5),
            handles: vec![HandleInfo {
                handle: HandleId(7),
                endpoint: EndpointName::new("kernel").unwrap(),
                actions: BTreeSet::from([ActionName::new("provide").unwrap()]),
                resource: ResourcePath::new(["endpoint", endpoint.as_str()]).unwrap(),
                delegable: false,
                expires_in_ms: None,
                max_calls: None,
            }],
        },
    )
    .unwrap();
    let ToKernel::Provide { request, authority, interface } = receive() else {
        panic!("expected publication")
    };
    assert_eq!((authority, &interface.endpoint), (HandleId(7), &endpoint));
    let action = &interface.actions[&ActionName::new(ACTION_GUIDE).unwrap()];
    assert_eq!(action.effect, EffectClass::Observation);
    write_frame(
        &mut input,
        &FromKernel::Response { request, result: Ok(ResponseBody::Bound { generation: Generation(1) }) },
    )
    .unwrap();
    write_frame(
        &mut input,
        &FromKernel::Dispatch {
            dispatch: DispatchId(1),
            endpoint: nucleus_protocol::EndpointName::new("plugin-development").unwrap(),
            caller: ProcessId(9),
            action: ActionName::new(ACTION_GUIDE).unwrap(),
            resource: ResourcePath::root(),
            payload: Payload::encode(&json!({})).unwrap(),
            remaining_ms: 1_000,
        },
    )
    .unwrap();
    let ToKernel::Reply { dispatch, result } = receive() else { panic!("expected reply") };
    assert_eq!(dispatch, DispatchId(1));
    assert_eq!(result.unwrap().decode::<Value>().unwrap(), json!({"guide":expected}));
    write_frame(&mut input, &FromKernel::Terminate).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "service did not terminate");
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
}
