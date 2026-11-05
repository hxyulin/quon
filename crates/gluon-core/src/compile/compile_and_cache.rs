//! Check, run and record one rustc invocation.
//!
//! Every rustc node (sysroot crate, config crate, build unit) goes
//! through [`compile_and_cache`]:
//!
//! 1. Check the job's fingerprint record (see
//!    [`crate::cache::store`]). The command hash is the builder's hash
//!    of program, argv, env and cwd; the toolchain is
//!    [`crate::compile::RustcInfo::toolchain_hash`]. Inputs discovered
//!    by the previous run (its depfile) come from the record, so a
//!    fresh check reads one file and stats the inputs.
//! 2. Fresh: replay the recorded stderr and return without spawning.
//! 3. Stale: report why under `-v`, spawn rustc, and on success run the
//!    optional post step (objcopy for raw binaries).
//! 4. Parse the depfile, then write the record.
//!
//! Any failure before step 4 leaves the old record, which no longer
//! matches, so the next build reruns the job. In particular a depfile
//! that cannot be parsed costs a rerun, never a stale hit.

use crate::cache::{Freshness, FreshnessQuery, RecordInput, parse_depfile};
use crate::compile::{CompileCtx, RustcCommandBuilder};
use crate::error::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Builds the error for a rustc run that exited non-zero. Called with
/// the process output and the rendered command line.
pub(crate) type RustcFailDiag = dyn FnOnce(&Output, &str) -> Error;

/// Builds the error for a depfile that could not be read after a
/// successful run.
pub(crate) type DepfileFailDiag = dyn FnOnce(&crate::error::Error) -> Error;

/// Runs after a successful rustc spawn and before the record is
/// written. `RawBinary` uses it to objcopy `<name>.elf` into
/// `<name>.bin`. A failure leaves the record stale, so the next build
/// reruns rustc and the step.
pub(crate) type PostCompileStep = Box<dyn FnOnce() -> Result<()>>;

/// One rustc invocation and how to check and record it.
pub(crate) struct RustcJob<'a> {
    /// Names the node in `-v` stale reasons.
    pub what: String,
    pub builder: RustcCommandBuilder,
    pub key: String,
    /// The path handed to dependents. Must exist for a hit.
    pub output_path: PathBuf,
    pub depfile_path: PathBuf,
    /// Created before the spawn.
    pub out_dir: Option<&'a Path>,
    /// Inputs known before the run that rustc's depfile may not list:
    /// direct deps' outputs, `artifact_env` artifacts. The builder's
    /// own file inputs (linker script, target spec) are added here.
    pub declared: Vec<PathBuf>,
    /// Other files that must exist for a hit, such as a raw binary's
    /// `.elf`.
    pub aux_outputs: Vec<PathBuf>,
    pub post_compile_step: Option<PostCompileStep>,
    pub rustc_fail: Box<RustcFailDiag>,
    pub depfile_fail: Box<DepfileFailDiag>,
}

/// Check, run and record `job`; see the module docs. Returns the
/// output path and whether the record was fresh (no spawn).
pub(crate) fn compile_and_cache(
    ctx: &CompileCtx,
    job: RustcJob<'_>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let RustcJob {
        what,
        builder,
        key,
        output_path,
        depfile_path,
        out_dir,
        mut declared,
        aux_outputs,
        post_compile_step,
        rustc_fail,
        depfile_fail,
    } = job;
    declared.extend(builder.file_inputs());
    declared.sort();
    declared.dedup();
    let command = builder.hash();
    let toolchain = ctx.toolchain();
    let mut outputs = Vec::with_capacity(1 + aux_outputs.len());
    outputs.push(output_path.clone());
    outputs.extend(aux_outputs);

    let query = FreshnessQuery {
        key: &key,
        command,
        toolchain,
        declared: &declared,
        outputs: &outputs,
        command_env: builder.envs(),
    };
    match ctx.store.check(&query) {
        Freshness::Fresh(fp) => {
            // Warnings are part of the build's output, as in Cargo:
            // a cache hit prints them again.
            stderr_sink.extend_from_slice(&ctx.store.stderr(&fp));
            return Ok((output_path, true));
        }
        Freshness::Stale(reason) => ctx.report_stale(stderr_sink, &what, &reason),
    }

    if let Some(dir) = out_dir {
        std::fs::create_dir_all(dir).map_err(|e| Error::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
    }

    // Rendered before `into_command` consumes the builder, for the
    // failure diagnostic.
    let rustc_path = builder.rustc_path().to_path_buf();
    let argv_snapshot = builder.args().to_vec();
    let render_cmd = || -> String {
        let mut s = rustc_path.display().to_string();
        for arg in &argv_snapshot {
            s.push(' ');
            s.push_str(&format!("{arg:?}"));
        }
        s
    };

    let mut cmd = builder.into_command();
    let started_at = std::time::SystemTime::now();
    let output = cmd.output().map_err(|e| Error::Io {
        path: rustc_path.clone(),
        source: e,
    })?;
    if !output.status.success() {
        return Err(rustc_fail(&output, &render_cmd()));
    }
    // The worker pool prints the job's buffer atomically when it
    // finishes, so parallel compiles never interleave. On failure the
    // diagnostic carries stderr instead.
    stderr_sink.extend_from_slice(&output.stderr);

    if let Some(post) = post_compile_step {
        post()?;
    }

    let depinfo = match parse_depfile(&depfile_path) {
        Ok(d) => d,
        Err(e) => return Err(depfile_fail(&e)),
    };
    ctx.store.record(RecordInput {
        key: &key,
        command,
        toolchain,
        declared: &declared,
        discovered: &depinfo.files,
        // The values rustc saw, as it recorded them.
        env: depinfo.env,
        outputs: &outputs,
        started_at,
        stderr: &output.stderr,
    })?;
    Ok((output_path, false))
}
