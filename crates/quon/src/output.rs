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
//! know. On a terminal, a `Building` line under them shows the jobs in
//! flight and is redrawn in place.

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use quon_core::{DriverKind, JobInfo, JobKind, JobOutcome, NodeId, Reporter};
use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::Write;
use std::time::{Duration, Instant};

const GREEN: Style = AnsiColor::Green.on_default().bold();
const RED: Style = AnsiColor::Red.on_default().bold();
const YELLOW: Style = AnsiColor::Yellow.on_default().bold();
const CYAN: Style = AnsiColor::Cyan.on_default().bold();
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
    /// time since then, so it includes evaluating `quon.rs`.
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
    /// (`built`, `checked`, `linted`); the counts follow Quon's cache
    /// summary.
    pub fn finished(&mut self, profile: &quon_core::model::ResolvedProfile, counts: Counts) {
        let opt = if profile.opt_level == quon_core::model::OptLevel::Zero {
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

    /// Print a failed command's error. Quon's own diagnostics carry
    /// their level, so they print as they are, with the level colored;
    /// other errors get an `error:` prefix and Cargo's `Caused by:`
    /// list.
    pub fn error(&mut self, e: &anyhow::Error) {
        if let Some(quon_core::Error::Diagnostics(diags)) = e.downcast_ref::<quon_core::Error>() {
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

    fn diagnostic(&mut self, d: &quon_core::Diagnostic) {
        let (word, style) = match d.level {
            quon_core::Level::Error => ("error", RED),
            quon_core::Level::Warning => ("warning", YELLOW),
            quon_core::Level::Note => ("note", BOLD),
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

    /// Whether to draw the progress line. Redrawing in place needs a
    /// terminal; in a log every redraw would be a new line.
    fn progress_enabled(&self) -> bool {
        !self.quiet
            && self.err.is_terminal()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
    }

    /// Replace the current line with `line`, which must fit the
    /// terminal width. Written past `AutoStream` because it would strip
    /// the erase-line code along with colors when color is off.
    fn draw_progress(&mut self, line: &str) {
        let _ = write!(std::io::stderr(), "\r\x1b[K{line}");
    }

    /// Erase the progress line so the next write starts at column 0.
    fn clear_progress(&mut self) {
        let _ = write!(std::io::stderr(), "\r\x1b[K");
    }
}

/// Terminal width of stderr, or 80 when it cannot be read.
fn stderr_width() -> usize {
    terminal_size::terminal_size_of(std::io::stderr()).map_or(80, |(w, _)| usize::from(w.0))
}

/// The counts on the `Finished` line.
pub struct Counts {
    pub noun: &'static str,
    pub built: usize,
    pub cached: usize,
}

/// Cargo's duration format: `0.08s`, `16.40s`, `1m 05s`.
pub fn elapsed(d: Duration) -> String {
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
///
/// Dropping the reporter erases the progress line, so a failed build's
/// error starts on a clean line.
pub struct BuildReporter<'a> {
    shell: &'a mut Shell,
    driver: DriverKind,
    /// `None` when stderr is not a terminal or under `-q`.
    progress: Option<Progress>,
}

/// State of the `Building` line.
#[derive(Default)]
struct Progress {
    total: usize,
    done: usize,
    /// Short names of the jobs in flight, in `NodeId` order so the line
    /// does not reorder as jobs finish.
    active: BTreeMap<NodeId, String>,
    /// Whether a line is on screen and must be erased before the next
    /// write.
    shown: bool,
}

impl<'a> BuildReporter<'a> {
    pub fn new(shell: &'a mut Shell, driver: DriverKind) -> Self {
        let progress = shell.progress_enabled().then(Progress::default);
        Self {
            shell,
            driver,
            progress,
        }
    }

    fn redraw(&mut self) {
        let Some(p) = &mut self.progress else { return };
        if p.active.is_empty() || p.total == 0 {
            return;
        }
        let line = progress_line(
            p.done,
            p.total,
            &p.active.values().map(String::as_str).collect::<Vec<_>>(),
            stderr_width(),
            self.shell.color,
        );
        self.shell.draw_progress(&line);
        p.shown = true;
    }

    fn clear(&mut self) {
        if let Some(p) = &mut self.progress
            && p.shown
        {
            self.shell.clear_progress();
            p.shown = false;
        }
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
            // full, also under `quon check`, because both are shared
            // with `quon build`.
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

/// The short name of a job on the progress line, as Cargo writes
/// `foo(build)` for a build script.
fn short_name(job: &JobInfo) -> String {
    match job.kind {
        JobKind::Sysroot => "sysroot".to_string(),
        JobKind::ConfigCrate => "config crate".to_string(),
        JobKind::BuildScript => format!("{}(build)", job.name),
        _ => job.name.clone(),
    }
}

/// `    Building [=====>      ] 3/6: kernel, alloc`, cut to fit
/// `width` columns. The last column stays empty: writing it makes some
/// terminals wrap, and the erase-line code then clears only the second
/// line.
fn progress_line(done: usize, total: usize, names: &[&str], width: usize, color: bool) -> String {
    const BAR: usize = 25;
    let filled = BAR * done / total.max(1);
    let bar = match filled {
        0 => " ".repeat(BAR),
        n => format!("{}>{}", "=".repeat(n - 1), " ".repeat(BAR - n)),
    };
    let mut line = format!(
        "{:>12} [{bar}] {done}/{total}: {}",
        "Building",
        names.join(", ")
    );
    let max = width.saturating_sub(1);
    if line.chars().count() > max {
        line = line.chars().take(max.saturating_sub(3)).collect();
        line.push_str("...");
    }
    if color && line.len() >= 12 {
        let (verb, rest) = line.split_at(12);
        line = format!("{CYAN}{verb}{CYAN:#}{rest}");
    }
    line
}

impl Reporter for BuildReporter<'_> {
    fn plan(&mut self, jobs: usize) {
        if let Some(p) = &mut self.progress {
            p.total = jobs;
        }
    }

    fn started(&mut self, node: NodeId, job: &JobInfo) {
        if let Some(p) = &mut self.progress {
            p.active.insert(node, short_name(job));
        }
        self.redraw();
    }

    fn finished(
        &mut self,
        node: NodeId,
        job: &JobInfo,
        outcome: JobOutcome,
        stdout: &[u8],
        stderr: &[u8],
    ) -> std::io::Result<()> {
        self.clear();
        if let Some(p) = &mut self.progress {
            p.active.remove(&node);
            p.done += 1;
        }
        if let Some((verb, message)) = self.describe(job) {
            match outcome {
                JobOutcome::Built | JobOutcome::Failed => self.shell.status(verb, message),
                // Cargo prints `Fresh` only under `-v`.
                JobOutcome::Cached if self.shell.verbose => self.shell.status("Fresh", message),
                JobOutcome::Cached => {}
            }
        }
        std::io::stdout().write_all(stdout)?;
        self.shell.write_raw(stderr)?;
        self.redraw();
        Ok(())
    }

    fn note(&mut self, message: &str) {
        self.clear();
        self.shell.note(message);
        self.redraw();
    }
}

impl Drop for BuildReporter<'_> {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_shows_bar_count_and_names() {
        let line = progress_line(3, 6, &["kernel", "alloc"], 120, false);
        assert_eq!(
            line,
            "    Building [===========>             ] 3/6: kernel, alloc"
        );
    }

    #[test]
    fn progress_line_fits_the_terminal() {
        let names = ["a_long_crate_name"; 10];
        let line = progress_line(1, 40, &names, 60, false);
        assert_eq!(line.chars().count(), 59);
        assert!(line.ends_with("..."), "{line}");
    }

    #[test]
    fn progress_line_colors_only_the_verb() {
        let line = progress_line(0, 2, &["kernel"], 120, true);
        let plain = anstream::adapter::strip_str(&line).to_string();
        assert_eq!(plain, progress_line(0, 2, &["kernel"], 120, false));
        assert!(line.starts_with(&format!("{CYAN}")), "{line:?}");
    }

    #[test]
    fn elapsed_matches_cargo() {
        assert_eq!(elapsed(Duration::from_millis(80)), "0.08s");
        assert_eq!(elapsed(Duration::from_millis(16_404)), "16.40s");
        assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    }
}
