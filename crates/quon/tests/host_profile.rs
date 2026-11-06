//! End-to-end CLI tests for a host profile against the `host-tool`
//! fixture: a project that declares no target, builds for the host,
//! and runs its binary directly.

mod support;

#[test]
#[ignore]
fn host_profile_builds_without_a_sysroot_and_runs_directly() {
    if !support::rustc_available("host_profile_builds_without_a_sysroot_and_runs_directly") {
        return;
    }
    let (_tmp, fixture) = support::setup_fixture("host-tool");

    support::quon_ok(&fixture, &["build"]);
    let build = fixture.join("build");
    assert!(build.join("host").is_dir(), "no host output");
    assert!(
        !build.join("sysroot").exists(),
        "a host profile built a sysroot"
    );
    assert!(
        !build.join("cross").exists(),
        "a host profile built for a target"
    );

    let out = support::quon(&fixture)
        .args(["run", "--no-build", "--", "a", "b"])
        .env_remove("HELLO_EXIT")
        .output()
        .expect("spawn quon run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout.trim(), "hello a b");
    assert!(
        stderr.contains("Exited runner 'host' with status 0"),
        "{stderr}"
    );

    let out = support::quon(&fixture)
        .args(["run", "--no-build"])
        .env("HELLO_EXIT", "3")
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(
        stderr.contains("runner 'host' exited with status 3"),
        "{stderr}"
    );

    // QEMU-only flags are refused.
    let out = support::quon(&fixture)
        .args(["run", "--no-build", "--gdb"])
        .output()
        .expect("spawn quon run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("needs a QEMU runner"), "{stderr}");
}
