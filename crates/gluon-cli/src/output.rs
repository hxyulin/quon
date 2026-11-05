//! Everything the CLI writes to stderr: status lines, notes, errors and
//! the output of build jobs.
//!
//! One [`Shell`] per process decides, once, whether stderr takes color
//! and how chatty to be (`-q`, `-v`). Writes go through an
//! [`anstream::AutoStream`], which strips escape codes when color is
//! off. That matters for job output: rustc is asked for color whenever
//! the shell has it, and a cache hit replays the stderr recorded by an
//! earlier run, which may have colored it for a terminal.
//!
//! Status lines follow Cargo's layout (a right-aligned verb in a
//! 12-column field) so the output reads like the tool users already
//! know.

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use gluon_core::{DriverKind, JobInfo, JobKind, JobOutcome, NodeId, Reporter};
use std::fmt::Display;
use std::io::Write;
use std::time::{Duration, Instant};

const GREEN: Style = AnsiColor::Green.on_default().bold();
const RED: Style = AnsiColor::Red.on_default().bold();
const YELLOW: Style = AnsiColor::Yellow.on_default().bold();
const BOLD: Style = Style::new().bold();

/// The process's stderr, with the color and verbosity choices made once.
pub struct Shell {
    err: AutoStream<std::io::Stderr>,
    color: bool,
    quiet: bool,
    verbose: bool,
    started: Instant,
}

impl Shell {
    /// `started` is when the command began; the finish line reports the
    /// time since then, so it includes evaluating `gluon.rs`.
    pub fn new(choice: ColorChoice, quiet: bool, verbose: bool, started: Instant) -> Self {
        let err = AutoStream::new(std::io::stderr(), choice);
        // `current_choice` has already folded in NO_COLOR,
        // CLICOLOR_FORCE, TERM=dumb and whether stderr is a terminal.
        let color = err.current_choice() != ColorChoice::Never;
        Self {
            err,
            color,
            quiet,
            verbose,
            started,
        }
    }

    /// Whether stderr keeps escape codes. Rustc is asked for color
    /// exactly when this is true.
    pub fn color(&self) -> bool {
        self.color
    }

    /// `verb` right-aligned in green, then `message`, as Cargo prints
    /// `   Compiling foo v0.1.0`. Silent under `-q`.
    pub fn status(&mut self, verb: &str, message: impl Display) {
        self.status_styled(verb, GREEN, message);
    }

    fn status_styled(&mut self, verb: &str, style: Style, message: impl Display) {
        if self.quiet {
            return;
        }
        let _ = writeln!(self.err, "{style}{verb:>12}{style:#} {message}");
    }

    /// `note: <message>`. Silent under `-q`.
    pub fn note(&mut self, message: impl Display) {
        if self.quiet {
            return;
        }
        let _ = writeln!(self.err, "{BOLD}note{BOLD:#}: {message}");
    }

    /// The `Finished` line. `noun` is what each driver did to a crate
    /// (`built`, `checked`, `linted`); the counts follow Gluon's cache
    /// summary.
    pub fn finished(&mut self, profile: &gluon_core::model::ResolvedProfile, counts: Counts) {
        let opt = if profile.opt_level == 0 {
            "unoptimized"
        } else {
            "optimized"
        };
        let debug = if profile.debug_info {
            " + debuginfo"
        } else {
            ""
        };
        let Counts {
            noun,
            built,
            cached,
        } = counts;
        self.status(
            "Finished",
            format_args!(
                "`{}` profile [{opt}{debug}] target(s) in {} ({noun} {built}, cached {cached})",
                profile.name,
                elapsed(self.started.elapsed()),
            ),
        );
    }

    /// Print a failed command's error. Gluon's own diagnostics carry
    /// their level, so they print as they are, with the level colored;
    /// other errors get an `error:` prefix and Cargo's `Caused by:`
    /// list.
    pub fn error(&mut self, e: &anyhow::Error) {
        if let Some(gluon_core::Error::Diagnostics(diags)) = e.downcast_ref::<gluon_core::Error>() {
            for d in diags {
                self.diagnostic(d);
            }
            return;
        }
        let _ = writeln!(self.err, "{RED}error{RED:#}: {e}");
        let mut causes = e.chain().skip(1).peekable();
        if causes.peek().is_some() {
            let _ = writeln!(self.err, "\nCaused by:");
            for cause in causes {
                let _ = writeln!(self.err, "  {cause}");
            }
        }
    }

    fn diagnostic(&mut self, d: &gluon_core::Diagnostic) {
        let (word, style) = match d.level {
            gluon_core::Level::Error => ("error", RED),
            gluon_core::Level::Warning => ("warning", YELLOW),
            gluon_core::Level::Note => ("note", BOLD),
        };
        if let Some(span) = &d.span {
            let _ = write!(self.err, "{span}: ");
        }
        let _ = writeln!(self.err, "{style}{word}{style:#}: {}", d.message);
        for note in &d.notes {
            let _ = writeln!(self.err, "    {BOLD}note{BOLD:#}: {note}");
        }
    }

    /// Write bytes a job produced, stripping color if stderr takes none.
    fn write_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.err.write_all(bytes)
    }
}

/// The counts on the `Finished` line.
pub struct Counts {
    pub noun: &'static str,
    pub built: usize,
    pub cached: usize,
}

/// Cargo's duration format: `0.08s`, `16.40s`, `1m 05s`.
fn elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:02}s", secs, d.subsec_millis() / 10)
    }
}

/// Turns the worker pool's job events into status lines on a [`Shell`].
///
/// A line prints when a job finishes, not when it starts: whether a job
/// is a cache hit is known only after its freshness check, which runs
/// inside the job. Printing at the end means a cache hit never prints
/// `Compiling`. The job's own output follows its line, as rustc's
/// warnings follow Cargo's `Compiling` line.
pub struct BuildReporter<'a> {
    shell: &'a mut Shell,
    driver: DriverKind,
}

impl<'a> BuildReporter<'a> {
    pub fn new(shell: &'a mut Shell, driver: DriverKind) -> Self {
        Self { shell, driver }
    }

    /// The verb and message for a job that ran, or `None` for jobs that
    /// Cargo also leaves out (build scripts).
    fn describe(&self, job: &JobInfo) -> Option<(&'static str, String)> {
        let compile = match self.driver {
            DriverKind::Rustc => "Compiling",
            DriverKind::Check | DriverKind::Clippy => "Checking",
        };
        let name = &job.name;
        Some(match job.kind {
            JobKind::Crate if job.detail.is_empty() => (compile, name.clone()),
            JobKind::Crate => (compile, format!("{name} {}", job.detail)),
            // The sysroot and config crate are always compiled in
            // full, also under `gluon check`, because both are shared
            // with `gluon build`.
            JobKind::Sysroot => ("Compiling", format!("sysroot ({name})")),
            JobKind::ConfigCrate => ("Compiling", format!("config crate ({name})")),
            JobKind::BuildScript => return None,
            JobKind::Rule => ("Running", format!("rule {name}")),
            JobKind::BuildStep => ("Running", format!("build step {name}")),
            JobKind::Esp => ("Assembling", format!("esp {name}")),
            JobKind::Image => ("Assembling", format!("image {name}")),
            JobKind::Other => ("Running", name.clone()),
        })
    }
}

impl Reporter for BuildReporter<'_> {
    fn finished(
        &mut self,
        _node: NodeId,
        job: &JobInfo,
        outcome: JobOutcome,
        stdout: &[u8],
        stderr: &[u8],
    ) -> std::io::Result<()> {
        if let Some((verb, message)) = self.describe(job) {
            match outcome {
                JobOutcome::Built | JobOutcome::Failed => self.shell.status(verb, message),
                // Cargo prints `Fresh` only under `-v`.
                JobOutcome::Cached if self.shell.verbose => self.shell.status("Fresh", message),
                JobOutcome::Cached => {}
            }
        }
        std::io::stdout().write_all(stdout)?;
        self.shell.write_raw(stderr)
    }

    fn note(&mut self, message: &str) {
        self.shell.note(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_matches_cargo() {
        assert_eq!(elapsed(Duration::from_millis(80)), "0.08s");
        assert_eq!(elapsed(Duration::from_millis(16_404)), "16.40s");
        assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    }
}
