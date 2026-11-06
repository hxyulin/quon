//! The `quon-runner-qemu` executable answers the runner protocol the
//! same way the built-in runner answers in process.

use quon_runner::{Context, External, PrepareRequest, Runner};
use quon_runner_qemu::QemuRunner;
use serde_json::json;

fn request() -> PrepareRequest {
    PrepareRequest {
        context: Context {
            runner: "vm".into(),
            profile: "dev".into(),
            target: Some("x86_64-unknown-none".into()),
            build_dir: "/p/build".into(),
            settings: serde_json::from_value(json!({"machine": "pc", "test_timeout": 7})).unwrap(),
            overrides: serde_json::from_value(json!({"memory_mb": 64})).unwrap(),
            test: false,
        },
        binary: "/p/build/kernel".into(),
        extra_args: vec!["-no-reboot".into()],
        gdb: true,
        ..PrepareRequest::default()
    }
}

#[test]
fn the_executable_matches_the_built_in_runner() {
    let external = External::new(env!("CARGO_BIN_EXE_quon-runner-qemu"));
    let req = request();
    assert_eq!(
        external.validate(&req.context).unwrap(),
        QemuRunner.validate(&req.context).unwrap()
    );
    let reply = external.prepare(&req).unwrap();
    assert_eq!(reply, QemuRunner.prepare(&req).unwrap());
    assert!(
        reply.args.windows(2).any(|w| w == ["-m", "64M"]),
        "{reply:?}"
    );
}

#[test]
fn the_executable_reports_runner_errors() {
    let external = External::new(env!("CARGO_BIN_EXE_quon-runner-qemu"));
    let mut req = request();
    req.context.target = None;
    let err = external.validate(&req.context).unwrap_err();
    assert!(err.0.contains("booting under QEMU needs a target"), "{err}");
}
