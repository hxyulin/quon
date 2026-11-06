//! Boots `examples/limine-kernel` under QEMU with several Limine
//! releases. Each run fetches that release's pinned `BOOTX64.EFI`,
//! installs a config file in the syntax the release reads, and checks
//! that the kernel reports the expected bootloader version on the
//! serial port and exits QEMU with status 33 through `isa-debug-exit`,
//! which the example's `success_exit_code(33)` turns into a zero exit
//! from `quon run`.
//!
//! `#[ignore]`-gated like the other build tests. It skips when QEMU,
//! OVMF, the `rust-src` component or the network is missing. With Nix:
//!
//! ```sh
//! nix shell nixpkgs#qemu -c cargo test -p quon --test limine_boot -- --ignored
//! ```

mod support;

use std::fs;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::Command;

use std::time::Duration;
use support::{copy_dir_all, repo_root, seed_sysroot};

/// Config syntax. Limine 8 renamed `limine.cfg` to `limine.conf` and
/// changed its syntax.
enum Syntax {
    /// Limine 7: `limine.cfg`, `KEY=value`, `:Entry`.
    Cfg,
    /// Limine 8 and later: `limine.conf`, `key: value`, `/Entry`.
    Conf,
}

struct Release {
    version: &'static str,
    /// SHA-256 of `BOOTX64.EFI` at the `v<version>-binary` tag.
    sha256: &'static str,
    syntax: Syntax,
}

/// The latest release of each major version from 7 to 11, newest
/// first. Limine 8 and later ignore a `limine.cfg`, so the 7.x run comes
/// last and the shared ESP never offers 7.x a stale `limine.conf`.
const RELEASES: &[Release] = &[
    Release {
        version: "11.4.1",
        sha256: "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06",
        syntax: Syntax::Conf,
    },
    Release {
        version: "10.8.5",
        sha256: "91ef81c1f95c4ac8bf77c4393a0b2fcd2628550827d7abf89a43194400e17a85",
        syntax: Syntax::Conf,
    },
    Release {
        version: "9.6.7",
        sha256: "c1d6c34cf827e0c5cb0f3546d2db8568dd29aae26833a6332db5532b823503c3",
        syntax: Syntax::Conf,
    },
    Release {
        version: "8.7.0",
        sha256: "51ca2b20ed14bbc678a396cdfb0c1d941acbc155cabebc9bfafabe4cdae1be73",
        syntax: Syntax::Conf,
    },
    Release {
        version: "7.13.3",
        sha256: "e9071610898f6a40be55e0aa60226b2a31ba763daf494ab29affc0ceae17bb54",
        syntax: Syntax::Cfg,
    },
];

/// The lines of the example's quon.rs that the test rewrites.
const EXAMPLE_LIMINE_LINE: &str = r#"Download::limine("11.4.1", "BOOTX64.EFI", "333f7a69379b1f47e019be215885cdc078b9edd6e9255cbe2f706d6c5ec0ef06")"#;
const EXAMPLE_CONF_LINE: &str = r#".add("limine.conf", "limine.conf")"#;
const EXAMPLE_SUCCESS_LINE: &str = ".success_exit_code(33),";

/// Why the test cannot run here, if anything is missing.
fn missing_prerequisite() -> Option<String> {
    match quon_core::RustcInfo::probe() {
        Ok(info) if info.rust_src.is_some() => {}
        Ok(_) => return Some("rust-src component not installed".into()),
        Err(e) => return Some(format!("rustc probe failed: {e}")),
    }
    if Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .is_err()
    {
        return Some("qemu-system-x86_64 not on PATH".into());
    }
    let reachable = ("raw.githubusercontent.com", 443)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .is_some_and(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(10)).is_ok());
    if !reachable {
        return Some("raw.githubusercontent.com is unreachable".into());
    }
    None
}

fn config_file(release: &Release) -> (&'static str, String) {
    match release.syntax {
        Syntax::Cfg => (
            "limine.cfg",
            "TIMEOUT=0\nSERIAL=yes\nVERBOSE=yes\n\n\
             :Quon Limine kernel\n    PROTOCOL=limine\n    KERNEL_PATH=boot:///boot/kernel\n"
                .into(),
        ),
        Syntax::Conf => (
            "limine.conf",
            "timeout: 0\nserial: yes\nverbose: yes\n\n\
             /Quon Limine kernel\n    protocol: limine\n    path: boot():/boot/kernel\n"
                .into(),
        ),
    }
}

/// Run `quon run` in `project`; return the combined output and
/// whether `quon run` exited 0.
fn quon_run(project: &Path) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_quon"))
        .args(["run", "--timeout", "60", "--", "-display", "none"])
        .current_dir(project)
        .output()
        .expect("spawn quon run");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, out.status.success())
}

/// Point the project at `release` and run it; return the combined
/// output and whether it booted.
fn boot(project: &Path, template: &str, release: &Release) -> (String, bool) {
    let (conf_name, conf_text) = config_file(release);
    fs::write(project.join(conf_name), conf_text).unwrap();
    let quon_rs = template
        .replace(
            EXAMPLE_LIMINE_LINE,
            &format!(
                r#"Download::limine("{}", "BOOTX64.EFI", "{}")"#,
                release.version, release.sha256
            ),
        )
        .replace(
            EXAMPLE_CONF_LINE,
            &format!(r#".add("{conf_name}", "{conf_name}")"#),
        );
    fs::write(project.join("quon.rs"), quon_rs).unwrap();

    let (text, success) = quon_run(project);
    let banner = format!("limine-kernel: booted by Limine {}", release.version);
    (text.clone(), success && text.contains(&banner))
}

#[test]
#[ignore]
fn limine_releases_boot_the_example_kernel() {
    if let Some(reason) = missing_prerequisite() {
        eprintln!("limine_releases_boot_the_example_kernel: skipped — {reason}");
        return;
    }
    let tmp = tempfile::tempdir().expect("create tempdir");
    let project = tmp.path().join("limine-kernel");
    copy_dir_all(&repo_root().join("examples/limine-kernel"), &project).unwrap();
    seed_sysroot(&project);
    let template = fs::read_to_string(project.join("quon.rs")).unwrap();
    assert!(
        template.contains(EXAMPLE_LIMINE_LINE)
            && template.contains(EXAMPLE_CONF_LINE)
            && template.contains(EXAMPLE_SUCCESS_LINE),
        "examples/limine-kernel/quon.rs changed shape"
    );

    let mut failures = Vec::new();
    for release in RELEASES {
        let (output, booted) = boot(&project, &template, release);
        if output.contains("no OVMF firmware found") {
            eprintln!("limine_releases_boot_the_example_kernel: skipped — no OVMF firmware");
            return;
        }
        let banner = output
            .lines()
            .find_map(|l| l.find("limine-kernel:").map(|i| &l[i..]))
            .unwrap_or("no kernel output");
        eprintln!("Limine {}: {banner}", release.version);
        if !booted {
            failures.push(format!("Limine {}:\n{output}", release.version));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));

    // The same boot with a different success status must fail: status
    // 33 counts as success only because the config says so.
    let (conf_name, conf_text) = config_file(&RELEASES[0]);
    fs::write(project.join(conf_name), conf_text).unwrap();
    fs::write(
        project.join("quon.rs"),
        template.replace(EXAMPLE_SUCCESS_LINE, ".success_exit_code(35),"),
    )
    .unwrap();
    let (output, success) = quon_run(&project);
    assert!(
        !success,
        "quon run succeeded with the wrong status:\n{output}"
    );
    assert!(
        output.contains("runner 'qemu' exited with status 33, expected success status 35"),
        "{output}"
    );
}
