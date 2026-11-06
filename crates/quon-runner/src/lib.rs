//! The runner plugin protocol.
//!
//! A runner decides how `quon run`, `quon test` and the boot tests
//! start a built binary. Quon itself keeps the build, finding the
//! binary, the per-test ESP copies, timeouts, spawning and signals; a
//! runner plugin only turns a binary and its settings into a command.
//!
//! A plugin implements [`Runner`]. Quon calls it twice:
//!
//! 1. [`Runner::validate`] before the build, so a bad setting fails
//!    before minutes of compiling. The reply says how `quon test`
//!    boots a test binary in place of the run binary.
//! 2. [`Runner::prepare`] after the build, with the binary and the
//!    paths of the artifacts the build produced. The reply is the
//!    program to start, its arguments and environment, and the exit
//!    status that counts as success.
//!
//! Quon finds a plugin by name: among the runners built into the
//! `quon` binary first, then as `quon-runner-<name>` on `$PATH`. An
//! external plugin is started once per call. It reads one [`Envelope`]
//! as JSON on stdin, writes one [`Reply`] as JSON on stdout, and exits
//! 0; stderr goes to the terminal. [`serve`] does all of that for a
//! plugin's `main`:
//!
//! ```no_run
//! use quon_runner::*;
//!
//! struct Echo;
//!
//! impl Runner for Echo {
//!     fn validate(&self, _: &Context) -> Result<ValidateResponse, RunnerError> {
//!         Ok(ValidateResponse::default())
//!     }
//!
//!     fn prepare(&self, req: &PrepareRequest) -> Result<PrepareResponse, RunnerError> {
//!         Ok(PrepareResponse::new("echo", [req.binary.display().to_string()]))
//!     }
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     serve(&Echo)
//! }
//! ```
//!
//! Paths and arguments travel as UTF-8 strings, so a path that is not
//! valid UTF-8 cannot be passed to a plugin.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// The protocol version this crate speaks. An [`Envelope`] with another
/// version is refused, so a plugin built against an incompatible
/// version fails with a message instead of misreading a request.
pub const PROTOCOL_VERSION: u32 = 1;

/// What every call to a runner carries.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Context {
    /// The runner's name in `quon.rs`, for messages.
    pub runner: String,
    /// The resolved profile's name.
    pub profile: String,
    /// The profile's target: a triple for a built-in target, or the
    /// path of a JSON target spec. `None` for a host profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Quon's build directory. A runner may write files it needs under
    /// it, such as a writable copy of firmware.
    pub build_dir: PathBuf,
    /// The runner's settings from `quon.rs`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, serde_json::Value>,
    /// Settings that take precedence over `settings`: the profile's,
    /// then the command line's. A command-line boot mode flag arrives
    /// as `"boot_mode"`: `"direct"`, `"uefi"` or `"bios"`. How an
    /// override combines with the setting it overrides is up to the
    /// runner.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, serde_json::Value>,
    /// Set when `quon test` runs the binary, for an integration test
    /// or a boot test.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
}

/// The reply to [`Runner::validate`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateResponse {
    /// How `quon test` boots an integration test's binary.
    #[serde(default)]
    pub test_binary: TestBinary,
    /// Seconds a test may run when neither `--timeout` nor the
    /// profile's `test_timeout` says. `None` leaves Quon's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout: Option<u64>,
}

/// How an integration test's binary takes the run binary's place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TestBinary {
    /// Pass the test binary as [`PrepareRequest::binary`].
    #[default]
    AsBinary,
    /// Copy the ESP that holds the run binary, swap the test binary in,
    /// and pass the copy as [`PrepareRequest::boot_esp`]. For a
    /// firmware that loads the binary from the ESP.
    EspCopy,
    /// Integration tests cannot run with these settings.
    Unsupported { reason: String },
}

/// A request to [`Runner::prepare`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PrepareRequest {
    #[serde(flatten)]
    pub context: Context,
    /// The binary to run: the profile's run binary, or a test's.
    pub binary: PathBuf,
    /// Every ESP, image and build step the project declares, with the
    /// path the build put it at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    /// The ESP to boot in place of the declared one, set when
    /// [`TestBinary::EspCopy`] made a copy for a test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_esp: Option<PathBuf>,
    /// Arguments from the command line, after `--`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// `--gdb`: start the binary halted, waiting for a debugger.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gdb: bool,
    /// Whether the build ran. `false` under `--dry-run` and
    /// `--no-build`: artifact paths are where the build would put them
    /// and may not exist, so a runner should not insist that they do.
    pub built: bool,
}

/// An artifact the build produces, by its name in `quon.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub name: String,
    pub kind: ArtifactKind,
    /// `None` when the artifact is declared but the build did not
    /// produce it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// An `esp(...)` directory.
    Esp,
    /// An `image(...)` disk image.
    Image,
    /// A `build_step(...)` output file.
    BuildStep,
}

/// The reply to [`Runner::prepare`]: the process Quon starts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Variables set for the process, on top of Quon's environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// The exit status that counts as success. `None` means 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_exit_code: Option<i32>,
    /// Lines `quon run` prints before starting the process, such as
    /// where to attach a debugger.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl PrepareResponse {
    /// A response that runs `program` with `args`.
    pub fn new<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// A runner's error. Quon prints the message as it is, so it should
/// say what is wrong and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerError(pub String);

impl RunnerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RunnerError {}

/// A runner plugin.
pub trait Runner: Send + Sync {
    /// Check the settings before the build.
    fn validate(&self, context: &Context) -> Result<ValidateResponse, RunnerError>;

    /// Turn the built binary into the process to start.
    fn prepare(&self, request: &PrepareRequest) -> Result<PrepareResponse, RunnerError>;
}

/// One call to a runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Validate(Context),
    Prepare(PrepareRequest),
}

/// What Quon writes to an external plugin's stdin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: u32,
    pub request: Request,
}

/// What an external plugin writes to its stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply<T> {
    Ok(T),
    Error(String),
}

impl<T> From<Result<T, RunnerError>> for Reply<T> {
    fn from(r: Result<T, RunnerError>) -> Self {
        match r {
            Ok(v) => Reply::Ok(v),
            Err(e) => Reply::Error(e.0),
        }
    }
}

/// Answer one request: parse `input` as an [`Envelope`], call `runner`,
/// and return the [`Reply`] as JSON. A malformed request or another
/// protocol version gets an error reply.
pub fn handle(runner: &dyn Runner, input: &[u8]) -> Vec<u8> {
    // The version is read on its own first: a request from another
    // version may not parse as this version's request at all.
    #[derive(Deserialize)]
    struct Version {
        protocol: u32,
    }
    match serde_json::from_slice::<Version>(input) {
        Ok(v) if v.protocol != PROTOCOL_VERSION => {
            return error_reply(format!(
                "runner protocol version {} is not supported; this plugin speaks version \
                 {PROTOCOL_VERSION}",
                v.protocol
            ));
        }
        _ => {}
    }
    let envelope: Envelope = match serde_json::from_slice(input) {
        Ok(e) => e,
        Err(e) => return error_reply(format!("malformed runner request: {e}")),
    };
    let json = match &envelope.request {
        Request::Validate(c) => serde_json::to_vec(&Reply::from(runner.validate(c))),
        Request::Prepare(r) => serde_json::to_vec(&Reply::from(runner.prepare(r))),
    };
    json.expect("runner replies serialize")
}

fn error_reply(message: String) -> Vec<u8> {
    serde_json::to_vec(&Reply::<()>::Error(message)).expect("an error reply serializes")
}

/// Run a plugin binary: answer the request on stdin on stdout. Returns
/// failure only when stdin or stdout fail; an error the runner returns
/// is a reply like any other.
pub fn serve(runner: &dyn Runner) -> ExitCode {
    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        eprintln!("failed to read the runner request: {e}");
        return ExitCode::FAILURE;
    }
    let reply = handle(runner, &input);
    let mut out = std::io::stdout().lock();
    if let Err(e) = out.write_all(&reply).and_then(|()| out.flush()) {
        eprintln!("failed to write the runner reply: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// A runner plugin in another executable, called over the JSON protocol.
#[derive(Debug, Clone)]
pub struct External {
    program: PathBuf,
}

impl External {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Find `quon-runner-<plugin>` in the directories of `path`, a
    /// `$PATH`-style list.
    pub fn find(plugin: &str, path: &std::ffi::OsStr) -> Option<Self> {
        let name = format!("quon-runner-{plugin}{}", std::env::consts::EXE_SUFFIX);
        std::env::split_paths(path)
            .map(|dir| dir.join(&name))
            .find(|p| p.is_file())
            .map(Self::new)
    }

    fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T, RunnerError> {
        let shown = self.program.display();
        let fail = |what: &str, e: &dyn std::fmt::Display| {
            RunnerError(format!("runner plugin `{shown}`: {what}: {e}"))
        };
        let envelope = Envelope {
            protocol: PROTOCOL_VERSION,
            request,
        };
        let input = serde_json::to_vec(&envelope).map_err(|e| fail("cannot encode request", &e))?;
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| fail("failed to start", &e))?;
        // A plugin that exits without reading stdin closes the pipe;
        // its exit status then tells the user more than EPIPE would.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&input);
        }
        let output = child
            .wait_with_output()
            .map_err(|e| fail("failed to wait", &e))?;
        if !output.status.success() {
            return Err(fail("exited unsuccessfully", &output.status));
        }
        match serde_json::from_slice::<Reply<T>>(&output.stdout) {
            Ok(Reply::Ok(v)) => Ok(v),
            Ok(Reply::Error(message)) => Err(RunnerError(message)),
            Err(e) => Err(fail("malformed reply", &e)),
        }
    }
}

impl Runner for External {
    fn validate(&self, context: &Context) -> Result<ValidateResponse, RunnerError> {
        self.call(Request::Validate(context.clone()))
    }

    fn prepare(&self, request: &PrepareRequest) -> Result<PrepareResponse, RunnerError> {
        self.call(Request::Prepare(request.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixed;

    impl Runner for Fixed {
        fn validate(&self, c: &Context) -> Result<ValidateResponse, RunnerError> {
            if c.settings.contains_key("bad") {
                return Err(RunnerError::new("setting 'bad' is bad"));
            }
            Ok(ValidateResponse {
                test_binary: TestBinary::EspCopy,
                test_timeout: Some(9),
            })
        }

        fn prepare(&self, r: &PrepareRequest) -> Result<PrepareResponse, RunnerError> {
            let mut resp = PrepareResponse::new("run", [r.binary.display().to_string()]);
            resp.args.extend(r.extra_args.iter().cloned());
            Ok(resp)
        }
    }

    fn call(runner: &dyn Runner, envelope: serde_json::Value) -> serde_json::Value {
        let out = handle(runner, &serde_json::to_vec(&envelope).unwrap());
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn requests_round_trip_through_json() {
        let request = Request::Prepare(PrepareRequest {
            context: Context {
                runner: "vm".into(),
                profile: "dev".into(),
                target: Some("x86_64-unknown-none".into()),
                build_dir: "/p/build".into(),
                settings: [("memory_mb".to_string(), json!(128))].into(),
                overrides: [("boot_mode".to_string(), json!("uefi"))].into(),
                test: true,
            },
            binary: "/p/build/kernel".into(),
            artifacts: vec![Artifact {
                name: "boot".into(),
                kind: ArtifactKind::Esp,
                path: Some("/p/build/esp/boot".into()),
            }],
            boot_esp: None,
            extra_args: vec!["-d".into(), "int".into()],
            gdb: true,
            built: false,
        });
        let envelope = Envelope {
            protocol: PROTOCOL_VERSION,
            request,
        };
        let text = serde_json::to_value(&envelope).unwrap();
        assert_eq!(text["request"]["method"], "prepare");
        assert_eq!(text["request"]["params"]["runner"], "vm");
        assert_eq!(text["request"]["params"]["artifacts"][0]["kind"], "esp");
        let back: Envelope = serde_json::from_value(text).unwrap();
        assert_eq!(back, envelope);
    }

    #[test]
    fn handle_answers_validate_and_prepare() {
        let validate = json!({
            "protocol": 1,
            "request": {"method": "validate", "params": {"runner": "vm", "profile": "dev", "build_dir": "/b"}},
        });
        assert_eq!(
            call(&Fixed, validate),
            json!({"ok": {"test_binary": {"kind": "esp_copy"}, "test_timeout": 9}})
        );

        let prepare = json!({
            "protocol": 1,
            "request": {"method": "prepare", "params": {
                "runner": "vm", "profile": "dev", "build_dir": "/b",
                "binary": "/b/kernel", "extra_args": ["-x"], "built": true,
            }},
        });
        assert_eq!(
            call(&Fixed, prepare),
            json!({"ok": {"program": "run", "args": ["/b/kernel", "-x"]}})
        );
    }

    #[test]
    fn handle_replies_with_errors() {
        let bad = json!({
            "protocol": 1,
            "request": {"method": "validate", "params": {
                "runner": "vm", "profile": "dev", "build_dir": "/b", "settings": {"bad": true},
            }},
        });
        assert_eq!(call(&Fixed, bad), json!({"error": "setting 'bad' is bad"}));

        let version = json!({"protocol": 99, "request": {"method": "validate", "params": {}}});
        let reply = call(&Fixed, version);
        assert!(
            reply["error"].as_str().unwrap().contains("version 99"),
            "{reply}"
        );

        let reply: serde_json::Value =
            serde_json::from_slice(&handle(&Fixed, b"not json")).unwrap();
        assert!(
            reply["error"]
                .as_str()
                .unwrap()
                .starts_with("malformed runner request"),
            "{reply}"
        );
    }

    #[test]
    fn missing_reply_fields_take_their_defaults() {
        let v: ValidateResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(v, ValidateResponse::default());
        let p: PrepareResponse = serde_json::from_str(r#"{"program": "x"}"#).unwrap();
        assert_eq!(p, PrepareResponse::new("x", Vec::<String>::new()));
    }

    #[test]
    fn find_looks_for_the_prefixed_name_on_the_path() {
        let dir = std::env::temp_dir().join(format!("quon-runner-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(format!("quon-runner-fake{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&exe, "").unwrap();
        let path = std::env::join_paths(["/nonexistent".into(), dir.clone()]).unwrap();
        assert_eq!(External::find("fake", &path).unwrap().program(), exe);
        assert!(External::find("other", &path).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
