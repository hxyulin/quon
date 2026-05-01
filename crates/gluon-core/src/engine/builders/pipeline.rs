//! Rule, pipeline, qemu, and bootloader builders.

use super::builder_method;
use crate::engine::EngineState;
use crate::engine::conversions::array_to_string_vec;
use crate::error::Diagnostic;
use gluon_model::{
    BootMode, DiskSource, EspDef, EspEntry, EspSource, ImageDef, ImageEntry, ImageSource,
    PartitionDef, PartitionTable, PipelineDef, PipelineStep, RawSource, RawWrite, RuleDef,
    RuleHandler, SerialMode,
};
use rhai::{Dynamic, Engine, NativeCallContext, Position};
use std::path::PathBuf;

/// Default name for the project-level singleton pipeline. The script calls
/// `pipeline()` with no arguments and implicitly refers to this one.
const DEFAULT_PIPELINE_NAME: &str = "main";

// ---------------------------------------------------------------------------
// Builder types
// ---------------------------------------------------------------------------

/// Chainable builder returned by `rule("name")`.
#[derive(Clone)]
pub struct RuleBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by `pipeline()`. The pipeline is a singleton
/// keyed by [`DEFAULT_PIPELINE_NAME`]; calling `pipeline()` more than once
/// returns a builder pointing at the same pipeline (not a duplicate).
#[derive(Clone)]
pub struct PipelineBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    /// Always `false` for the singleton pipeline; retained for macro
    /// compatibility with the `builder_method!` short-circuit.
    is_duplicate: bool,
}

/// Chainable builder returned by `qemu()`. Mutates
/// [`gluon_model::QemuDef`] through typed field setters.
#[derive(Clone)]
pub struct QemuBuilder {
    state: EngineState,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by `bootloader(kind)`. Known options
/// (`entry_crate`, `protocol`, `config_file`) write to typed fields on
/// [`gluon_model::BootloaderDef`]; the generic `set(key, value)` escape
/// hatch routes unrecognised keys into
/// [`gluon_model::BootloaderDef::extras`] for forward compatibility.
#[derive(Clone)]
pub struct BootloaderBuilder {
    state: EngineState,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by `image("name")`. Constructs an
/// [`ImageDef`](gluon_model::ImageDef) in the model arena.
#[derive(Clone)]
pub struct ImageBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by [`ImageBuilder`]'s `partition(name,
/// start_lba, fs_type)` method. Mutates a specific
/// [`PartitionDef`](gluon_model::PartitionDef) inside the parent image
/// (identified by index — partitions are append-only during config
/// evaluation, so the index is stable).
///
/// Calling `.end()` returns an [`ImageBuilder`] for the same image so
/// the caller can keep chaining at the disk level.
#[derive(Clone)]
pub struct PartitionBuilder {
    state: EngineState,
    image_name: String,
    /// Index into `ImageDef::partitions`. Stable for the lifetime of
    /// the script because partitions are only ever pushed onto the
    /// vector — never reordered or removed.
    partition_index: usize,
    pos: Position,
    is_duplicate: bool,
}

/// Chainable builder returned by `esp("name")`. Appends
/// `(source_crate, dest_path)` entries into [`gluon_model::EspDef::entries`]
/// via `.add(...)` calls. Duplicate declarations (two `esp("name")` calls
/// with the same name) emit a diagnostic and the second call short-circuits.
#[derive(Clone)]
pub struct EspBuilder {
    state: EngineState,
    name: String,
    pos: Position,
    is_duplicate: bool,
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

pub(super) fn register(engine: &mut Engine, state: &EngineState) {
    register_rule(engine, state.clone());
    register_pipeline(engine, state.clone());
    register_qemu(engine, state.clone());
    register_bootloader(engine, state.clone());
    register_esp(engine, state.clone());
    register_image(engine, state.clone());
}

// ---------------------------------------------------------------------------
// rule()
// ---------------------------------------------------------------------------

fn register_rule(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "rule",
        move |ctx: NativeCallContext, name: &str| -> RuleBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let (_, inserted) = s.model.borrow_mut().rules.insert(
                name.into(),
                RuleDef {
                    name: name.into(),
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                    depends_on: Vec::new(),
                    handler: RuleHandler::Builtin("exec".into()),
                    working_dir: None,
                    span: Some(span.clone()),
                },
            );
            if !inserted {
                s.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' is defined more than once"))
                        .with_span(span),
                );
            }
            RuleBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate: !inserted,
            }
        },
    );

    builder_method!(
        engine,
        "handler",
        RuleBuilder,
        |state, model, name, pos, handler: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.handler = RuleHandler::Builtin(handler.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "on_execute",
        RuleBuilder,
        |state, model, name, pos, script: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.handler = RuleHandler::Script(script.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "working_dir",
        RuleBuilder,
        |state, model, name, pos, dir: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.working_dir = Some(dir.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "inputs",
        RuleBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(h) = model.rules.lookup(name) {
                        if let Some(r) = model.rules.get_mut(h) {
                            r.inputs.append(&mut v);
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' inputs: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "outputs",
        RuleBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(h) = model.rules.lookup(name) {
                        if let Some(r) = model.rules.get_mut(h) {
                            r.outputs.append(&mut v);
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' outputs: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "output",
        RuleBuilder,
        |state, model, name, pos, single: &str| {
            let _ = (state, pos);
            if let Some(h) = model.rules.lookup(name) {
                if let Some(r) = model.rules.get_mut(h) {
                    r.outputs.push(single.into());
                }
            }
        }
    );

    builder_method!(
        engine,
        "depends_on",
        RuleBuilder,
        |state, model, name, pos, list: rhai::Array| {
            match array_to_string_vec(list) {
                Ok(mut v) => {
                    if let Some(h) = model.rules.lookup(name) {
                        if let Some(r) = model.rules.get_mut(h) {
                            r.depends_on.append(&mut v);
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("rule '{name}' depends_on: {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );
}

// ---------------------------------------------------------------------------
// pipeline() — singleton
// ---------------------------------------------------------------------------

fn register_pipeline(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "pipeline",
        move |ctx: NativeCallContext| -> PipelineBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            {
                let mut model = s.model.borrow_mut();
                if model.pipelines.lookup(DEFAULT_PIPELINE_NAME).is_none() {
                    model.pipelines.insert(
                        DEFAULT_PIPELINE_NAME.into(),
                        PipelineDef {
                            name: DEFAULT_PIPELINE_NAME.into(),
                            stages: Vec::new(),
                            span: Some(span),
                        },
                    );
                }
            }
            PipelineBuilder {
                state: s.clone(),
                name: DEFAULT_PIPELINE_NAME.into(),
                pos,
                is_duplicate: false,
            }
        },
    );

    builder_method!(
        engine,
        "stage",
        PipelineBuilder,
        |state, model, name, pos, stage_name: &str, inputs: rhai::Array| {
            match array_to_string_vec(inputs) {
                Ok(v) => {
                    if let Some(h) = model.pipelines.lookup(name) {
                        if let Some(p) = model.pipelines.get_mut(h) {
                            p.stages.push(PipelineStep {
                                name: stage_name.into(),
                                inputs: v,
                                inputs_handles: Vec::new(),
                                rule: None,
                            });
                        }
                    }
                }
                Err(msg) => state.push_diagnostic(
                    Diagnostic::error(format!("pipeline stage '{stage_name}': {msg}"))
                        .with_span(state.span_from(pos)),
                ),
            }
        }
    );

    builder_method!(
        engine,
        "rule",
        PipelineBuilder,
        |state, model, name, pos, rule_name: &str| {
            let _ = (state, pos);
            if let Some(h) = model.pipelines.lookup(name) {
                if let Some(p) = model.pipelines.get_mut(h) {
                    if let Some(last) = p.stages.last_mut() {
                        last.rule = Some(rule_name.into());
                    }
                }
            }
        }
    );

    builder_method!(
        engine,
        "barrier",
        PipelineBuilder,
        |state, model, name, pos, barrier_name: &str| {
            let _ = (state, pos);
            if let Some(h) = model.pipelines.lookup(name) {
                if let Some(p) = model.pipelines.get_mut(h) {
                    p.stages.push(PipelineStep {
                        name: barrier_name.into(),
                        inputs: Vec::new(),
                        inputs_handles: Vec::new(),
                        rule: None,
                    });
                }
            }
        }
    );
}

// ---------------------------------------------------------------------------
// qemu() — typed builder, mutates gluon_model::QemuDef directly.
// ---------------------------------------------------------------------------

fn register_qemu(engine: &mut Engine, state: EngineState) {
    let s = state;

    // qemu() with no args — binary stays None, filled in at resolve time.
    let s1 = s.clone();
    engine.register_fn("qemu", move |ctx: NativeCallContext| -> QemuBuilder {
        start_qemu(&s1, ctx.call_position(), None)
    });

    // qemu("qemu-system-x86_64") — binary set up front.
    let s2 = s.clone();
    engine.register_fn(
        "qemu",
        move |ctx: NativeCallContext, binary: &str| -> QemuBuilder {
            start_qemu(&s2, ctx.call_position(), Some(binary.into()))
        },
    );

    engine.register_fn(
        "binary",
        |builder: &mut QemuBuilder, binary: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            with_qemu(builder, |q| q.binary = Some(binary.into()));
            builder.clone()
        },
    );

    engine.register_fn(
        "machine",
        |builder: &mut QemuBuilder, machine: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            with_qemu(builder, |q| q.machine = Some(machine.into()));
            builder.clone()
        },
    );

    engine.register_fn(
        "memory",
        |builder: &mut QemuBuilder, mb: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if let Some(v) = clamp_u32(mb, "memory", builder) {
                with_qemu(builder, |q| q.memory_mb = Some(v));
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "cores",
        |builder: &mut QemuBuilder, n: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if let Some(v) = clamp_u32(n, "cores", builder) {
                with_qemu(builder, |q| q.cores = Some(v));
            }
            builder.clone()
        },
    );

    engine.register_fn("serial_stdio", |builder: &mut QemuBuilder| -> QemuBuilder {
        if builder.is_duplicate {
            return builder.clone();
        }
        with_qemu(builder, |q| q.serial = Some(SerialMode::Stdio));
        builder.clone()
    });

    engine.register_fn("serial_none", |builder: &mut QemuBuilder| -> QemuBuilder {
        if builder.is_duplicate {
            return builder.clone();
        }
        with_qemu(builder, |q| q.serial = Some(SerialMode::None));
        builder.clone()
    });

    engine.register_fn(
        "serial_file",
        |builder: &mut QemuBuilder, path: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let p = PathBuf::from(path);
            with_qemu(builder, |q| q.serial = Some(SerialMode::File(p)));
            builder.clone()
        },
    );

    engine.register_fn(
        "extra_args",
        |builder: &mut QemuBuilder, list: rhai::Array| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            match array_to_string_vec(list) {
                Ok(mut v) => with_qemu(builder, |q| q.extra_args.append(&mut v)),
                Err(msg) => builder.state.push_diagnostic(
                    Diagnostic::error(format!("qemu.extra_args: {msg}"))
                        .with_span(builder.state.span_from(builder.pos)),
                ),
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "boot_mode",
        |builder: &mut QemuBuilder, mode: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let parsed = match mode {
                "direct" => Some(BootMode::Direct),
                "uefi" => Some(BootMode::Uefi),
                "bios" => Some(BootMode::Bios),
                other => {
                    builder.state.push_diagnostic(
                        Diagnostic::error(format!(
                            "qemu.boot_mode: expected \"direct\", \"uefi\", or \"bios\", \
                             got \"{other}\""
                        ))
                        .with_span(builder.state.span_from(builder.pos)),
                    );
                    None
                }
            };
            if let Some(m) = parsed {
                with_qemu(builder, |q| q.boot_mode = Some(m));
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "ovmf_code",
        |builder: &mut QemuBuilder, path: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let p = PathBuf::from(path);
            with_qemu(builder, |q| q.ovmf_code = Some(p));
            builder.clone()
        },
    );

    engine.register_fn(
        "ovmf_vars",
        |builder: &mut QemuBuilder, path: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let p = PathBuf::from(path);
            with_qemu(builder, |q| q.ovmf_vars = Some(p));
            builder.clone()
        },
    );

    engine.register_fn(
        "esp_dir",
        |builder: &mut QemuBuilder, path: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_esp(builder, EspSource::Dir(PathBuf::from(path)));
            builder.clone()
        },
    );

    engine.register_fn(
        "esp_image",
        |builder: &mut QemuBuilder, path: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_esp(builder, EspSource::Image(PathBuf::from(path)));
            builder.clone()
        },
    );

    // -----------------------------------------------------------------
    // Boot-disk wiring (mode-agnostic).
    //
    // `disk_image("path/to/disk.img")` for a pre-built disk on the host
    // filesystem, and `disk_ref("name")` for a reference to an
    // `image("name")` declared in the same script — that one resolves
    // to whichever path the scheduler produces for the active build.
    //
    // We keep these as separate methods rather than a tagged-value
    // helper (`disk(image_ref(...))`) because Rhai's tagged values are
    // awkward to express precisely and don't compose well with the
    // diagnostic span attached to the *call* site.
    // -----------------------------------------------------------------
    engine.register_fn(
        "disk_image",
        |builder: &mut QemuBuilder, path: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            set_disk(
                builder,
                DiskSource::Image {
                    path: PathBuf::from(path),
                },
            );
            builder.clone()
        },
    );

    engine.register_fn(
        "disk_ref",
        |builder: &mut QemuBuilder, name: &str| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if name.is_empty() {
                builder.state.push_diagnostic(
                    Diagnostic::error("qemu.disk_ref: image name must not be empty".to_string())
                        .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            set_disk(
                builder,
                DiskSource::ImageRef {
                    name: name.to_string(),
                },
            );
            builder.clone()
        },
    );

    engine.register_fn(
        "test_exit_port",
        |builder: &mut QemuBuilder, port: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if !(0..=u16::MAX as i64).contains(&port) {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "qemu.test_exit_port: value {port} out of range (0..=65535)"
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
            } else {
                with_qemu(builder, |q| q.test_exit_port = Some(port as u16));
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "test_success_code",
        |builder: &mut QemuBuilder, code: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if !(0..=u8::MAX as i64).contains(&code) {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "qemu.test_success_code: value {code} out of range (0..=255)"
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
            } else {
                with_qemu(builder, |q| q.test_success_code = Some(code as u8));
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "test_timeout",
        |builder: &mut QemuBuilder, timeout: i64| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if let Some(v) = clamp_u32(timeout, "test_timeout", builder) {
                with_qemu(builder, |q| q.test_timeout = Some(v));
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "test_extra_args",
        |builder: &mut QemuBuilder, args: rhai::Array| -> QemuBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            with_qemu(builder, |q| {
                q.test_extra_args = args
                    .into_iter()
                    .filter_map(|v| v.into_string().ok())
                    .collect();
            });
            builder.clone()
        },
    );
}

// --- qemu() helpers ---

fn start_qemu(s: &EngineState, pos: Position, binary: Option<String>) -> QemuBuilder {
    let span = s.span_from(pos);
    let mut defined = s.qemu_defined.borrow_mut();
    let is_duplicate = *defined;
    if is_duplicate {
        s.push_diagnostic(
            Diagnostic::error("qemu is defined more than once".to_string()).with_span(span),
        );
    } else {
        *defined = true;
        let mut model = s.model.borrow_mut();
        if binary.is_some() {
            model.qemu.binary = binary;
        }
        if model.qemu.span.is_none() {
            model.qemu.span = Some(s.span_from(pos));
        }
    }
    QemuBuilder {
        state: s.clone(),
        pos,
        is_duplicate,
    }
}

fn with_qemu<F: FnOnce(&mut gluon_model::QemuDef)>(builder: &QemuBuilder, f: F) {
    let mut model = builder.state.model.borrow_mut();
    f(&mut model.qemu);
}

fn clamp_u32(value: i64, field: &str, builder: &QemuBuilder) -> Option<u32> {
    if !(0..=u32::MAX as i64).contains(&value) {
        builder.state.push_diagnostic(
            Diagnostic::error(format!(
                "qemu.{field}: value {value} out of range (0..=4294967295)"
            ))
            .with_span(builder.state.span_from(builder.pos)),
        );
        None
    } else {
        Some(value as u32)
    }
}

fn set_esp(builder: &QemuBuilder, esp: EspSource) {
    let mut model = builder.state.model.borrow_mut();
    if model.qemu.esp.is_some() {
        drop(model);
        builder.state.push_diagnostic(
            Diagnostic::error(
                "qemu: esp_dir / esp_image may only be called once per profile".to_string(),
            )
            .with_span(builder.state.span_from(builder.pos)),
        );
        return;
    }
    model.qemu.esp = Some(esp);
}

fn set_disk(builder: &QemuBuilder, disk: DiskSource) {
    let mut model = builder.state.model.borrow_mut();
    if model.qemu.disk.is_some() {
        drop(model);
        builder.state.push_diagnostic(
            Diagnostic::error(
                "qemu: disk_image / disk_ref may only be called once per profile".to_string(),
            )
            .with_span(builder.state.span_from(builder.pos)),
        );
        return;
    }
    model.qemu.disk = Some(disk);
}

// ---------------------------------------------------------------------------
// bootloader() — stub
// ---------------------------------------------------------------------------

fn register_bootloader(engine: &mut Engine, state: EngineState) {
    let s = state;
    engine.register_fn(
        "bootloader",
        move |ctx: NativeCallContext, kind: &str| -> BootloaderBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            let mut defined = s.bootloader_defined.borrow_mut();
            let is_duplicate = *defined;
            if is_duplicate {
                s.push_diagnostic(
                    Diagnostic::error("bootloader is defined more than once".to_string())
                        .with_span(span),
                );
            } else {
                *defined = true;
                let mut model = s.model.borrow_mut();
                model.bootloader = gluon_model::BootloaderDef {
                    kind: kind.into(),
                    span: Some(span.clone()),
                    ..Default::default()
                };
            }
            BootloaderBuilder {
                state: s.clone(),
                pos,
                is_duplicate,
            }
        },
    );

    fn set_extra(builder: &BootloaderBuilder, key: &str, val: String) {
        let mut model = builder.state.model.borrow_mut();
        model.bootloader.extras.insert(key.into(), val);
    }

    engine.register_fn(
        "entry_crate",
        |builder: &mut BootloaderBuilder, name: &str| -> BootloaderBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            model.bootloader.entry_crate = Some(name.into());
            builder.clone()
        },
    );

    engine.register_fn(
        "protocol",
        |builder: &mut BootloaderBuilder, proto: &str| -> BootloaderBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            model.bootloader.protocol = Some(proto.into());
            builder.clone()
        },
    );

    engine.register_fn(
        "config_file",
        |builder: &mut BootloaderBuilder, file: &str| -> BootloaderBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            model.bootloader.config_file = Some(file.into());
            builder.clone()
        },
    );

    // Generic string-valued .set(key, value) escape hatch for the stub.
    engine.register_fn(
        "set",
        |builder: &mut BootloaderBuilder, key: &str, value: Dynamic| -> BootloaderBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let observed = value.type_name();
            match value.into_string() {
                Ok(s) => set_extra(builder, key, s),
                Err(_) => builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "bootloader.set('{key}'): value must be a string, got {observed}"
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                ),
            }
            builder.clone()
        },
    );
}

// ---------------------------------------------------------------------------
// esp("name") — builder that describes an EFI System Partition to
// assemble from compiled artifacts. Supports multiple named ESPs per
// project (different bootloaders / boot stages can each have their own).
// ---------------------------------------------------------------------------

fn register_esp(engine: &mut Engine, state: EngineState) {
    let s = state;
    let s1 = s.clone();
    engine.register_fn(
        "esp",
        move |ctx: NativeCallContext, name: &str| -> EspBuilder {
            start_esp(&s1, ctx.call_position(), name)
        },
    );

    // `.add("crate-name", "EFI/BOOT/BOOTX64.EFI")`
    // Appends an entry to the ESP. The source crate is resolved later
    // by the scheduler (no handle on the model-side yet — that happens
    // in the intern pass).
    engine.register_fn(
        "add",
        |builder: &mut EspBuilder, source_crate: &str, dest_path: &str| -> EspBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if source_crate.is_empty() {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "esp(\"{}\").add: source crate name must not be empty",
                        builder.name
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            if dest_path.is_empty() {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "esp(\"{}\").add: destination path must not be empty",
                        builder.name
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            // Leading slashes in the dest path would resolve outside the
            // ESP root when joined, which is almost certainly a bug.
            // ESP paths are relative to the partition root by convention
            // (e.g. "EFI/BOOT/BOOTX64.EFI"), not rooted absolutes.
            if dest_path.starts_with('/') || dest_path.starts_with('\\') {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "esp(\"{}\").add: destination path '{}' must be relative to the ESP root (no leading '/')",
                        builder.name, dest_path
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }

            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.esps.lookup(&builder.name) {
                if let Some(esp) = model.esps.get_mut(h) {
                    esp.entries.push(EspEntry {
                        source_crate: source_crate.into(),
                        source_crate_handle: None,
                        dest_path: dest_path.into(),
                    });
                }
            }
            drop(model);
            builder.clone()
        },
    );
}

// --- esp() helpers ---

fn start_esp(s: &EngineState, pos: Position, name: &str) -> EspBuilder {
    let span = s.span_from(pos);
    if name.is_empty() {
        s.push_diagnostic(
            Diagnostic::error("esp() name must not be empty".to_string()).with_span(span.clone()),
        );
    }
    let mut model = s.model.borrow_mut();
    let (_h, inserted) = model.esps.insert(
        name.into(),
        EspDef {
            name: name.into(),
            entries: Vec::new(),
            span: Some(span.clone()),
        },
    );
    let is_duplicate = !inserted;
    drop(model);
    if is_duplicate {
        s.push_diagnostic(
            Diagnostic::error(format!("esp(\"{name}\") is defined more than once")).with_span(span),
        );
    }
    EspBuilder {
        state: s.clone(),
        name: name.into(),
        pos,
        is_duplicate,
    }
}

// ---------------------------------------------------------------------------
// image("name") — builder that describes a disk image to assemble from
// build artifacts. Supports multiple named images per project.
// ---------------------------------------------------------------------------

fn register_image(engine: &mut Engine, state: EngineState) {
    let s = state.clone();
    engine.register_fn(
        "image",
        move |ctx: NativeCallContext, name: &str| -> ImageBuilder {
            let pos = ctx.call_position();
            let span = s.span_from(pos);
            if name.is_empty() {
                s.push_diagnostic(
                    Diagnostic::error("image() name must not be empty".to_string())
                        .with_span(span.clone()),
                );
            }
            let mut model = s.model.borrow_mut();
            let (_h, inserted) = model.images.insert(
                name.into(),
                ImageDef {
                    name: name.into(),
                    span: Some(span.clone()),
                    ..Default::default()
                },
            );
            let is_duplicate = !inserted;
            drop(model);
            if is_duplicate {
                s.push_diagnostic(
                    Diagnostic::error(format!("image(\"{name}\") is defined more than once"))
                        .with_span(span),
                );
            }
            ImageBuilder {
                state: s.clone(),
                name: name.into(),
                pos,
                is_duplicate,
            }
        },
    );

    engine.register_fn(
        "format",
        |builder: &mut ImageBuilder, fmt: &str| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.format = Some(fmt.into());
                }
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "size",
        |builder: &mut ImageBuilder, mb: i64| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if mb <= 0 {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").size: must be positive, got {mb}",
                        builder.name
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.size_mb = Some(mb as u32);
                }
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "add_crate",
        |builder: &mut ImageBuilder, crate_name: &str, dest: &str| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.entries.push(ImageEntry {
                        source: ImageSource::Crate(crate_name.into()),
                        dest_path: dest.into(),
                    });
                }
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "add_file",
        |builder: &mut ImageBuilder, path: &str, dest: &str| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.entries.push(ImageEntry {
                        source: ImageSource::File(path.into()),
                        dest_path: dest.into(),
                    });
                }
            }
            builder.clone()
        },
    );

    engine.register_fn(
        "add_esp",
        |builder: &mut ImageBuilder, esp_name: &str, dest: &str| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.entries.push(ImageEntry {
                        source: ImageSource::Esp(esp_name.into()),
                        dest_path: dest.into(),
                    });
                }
            }
            builder.clone()
        },
    );

    // -----------------------------------------------------------------
    // Partition-table primitives (Phase 3).
    //
    // `image(...).partition_table("mbr")` switches the image into
    // partitioned-disk mode. After this, `add_crate` / `add_file` /
    // `add_esp` (which target the flat-FS root) are still legal but
    // produce a validation error at image-build time — the caller is
    // expected to use `.partition(...)` to scope file placement.
    // -----------------------------------------------------------------

    engine.register_fn(
        "partition_table",
        |builder: &mut ImageBuilder, kind: &str| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let table = match kind.to_ascii_lowercase().as_str() {
                "mbr" => PartitionTable::Mbr,
                "gpt" => {
                    builder.state.push_diagnostic(
                        Diagnostic::error(format!(
                            "image(\"{}\").partition_table(\"gpt\"): GPT is not implemented yet. \
                             Use \"mbr\" for now (the only Phase 3 supported value).",
                            builder.name
                        ))
                        .with_span(builder.state.span_from(builder.pos)),
                    );
                    return builder.clone();
                }
                other => {
                    builder.state.push_diagnostic(
                        Diagnostic::error(format!(
                            "image(\"{}\").partition_table(\"{other}\"): unknown partition table. \
                             Supported: \"mbr\".",
                            builder.name
                        ))
                        .with_span(builder.state.span_from(builder.pos)),
                    );
                    return builder.clone();
                }
            };
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.partition_table = Some(table);
                }
            }
            builder.clone()
        },
    );

    // Raw byte placements at LBA offsets. We expose three source
    // flavours via separate methods rather than tagged values because
    // Rhai's tagged-value ergonomics are awkward — keeping each source
    // kind as its own method preserves chained-builder readability:
    //
    //   .write_crate_at(0, "bios-boot-sector")
    //   .write_file_at(1, "boot/stage1_meta.bin")
    //   .write_bytes_at(2, [0xCC, 0xDD, ...])
    //
    // `.max_sectors(n)` retroactively bounds the last `write_*_at`.
    let s_wca = state.clone();
    engine.register_fn(
        "write_crate_at",
        move |ctx: NativeCallContext,
              builder: &mut ImageBuilder,
              lba: i64,
              crate_name: &str|
              -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let span = s_wca.span_from(ctx.call_position());
            if lba < 0 {
                s_wca.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").write_crate_at: lba must be >= 0, got {lba}",
                        builder.name
                    ))
                    .with_span(span),
                );
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.raw_writes.push(RawWrite {
                        lba: lba as u64,
                        source: RawSource::Crate {
                            name: crate_name.into(),
                        },
                        max_sectors: None,
                        span: Some(span),
                    });
                }
            }
            builder.clone()
        },
    );

    let s_wfa = state.clone();
    engine.register_fn(
        "write_file_at",
        move |ctx: NativeCallContext,
              builder: &mut ImageBuilder,
              lba: i64,
              path: &str|
              -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let span = s_wfa.span_from(ctx.call_position());
            if lba < 0 {
                s_wfa.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").write_file_at: lba must be >= 0, got {lba}",
                        builder.name
                    ))
                    .with_span(span),
                );
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.raw_writes.push(RawWrite {
                        lba: lba as u64,
                        source: RawSource::File { path: path.into() },
                        max_sectors: None,
                        span: Some(span),
                    });
                }
            }
            builder.clone()
        },
    );

    let s_wba = state.clone();
    engine.register_fn(
        "write_bytes_at",
        move |ctx: NativeCallContext,
              builder: &mut ImageBuilder,
              lba: i64,
              bytes: rhai::Array|
              -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            let span = s_wba.span_from(ctx.call_position());
            if lba < 0 {
                s_wba.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").write_bytes_at: lba must be >= 0, got {lba}",
                        builder.name
                    ))
                    .with_span(span),
                );
                return builder.clone();
            }
            // Each Rhai array element should be a small non-negative
            // integer. Validate so that an accidental string or float
            // doesn't get silently coerced.
            let mut payload = Vec::with_capacity(bytes.len());
            for (i, v) in bytes.into_iter().enumerate() {
                let n = match v.as_int() {
                    Ok(n) => n,
                    Err(actual) => {
                        s_wba.push_diagnostic(
                            Diagnostic::error(format!(
                                "image(\"{}\").write_bytes_at: byte index {i} is not an integer (got {actual})",
                                builder.name
                            ))
                            .with_span(span.clone()),
                        );
                        return builder.clone();
                    }
                };
                if !(0..=255).contains(&n) {
                    s_wba.push_diagnostic(
                        Diagnostic::error(format!(
                            "image(\"{}\").write_bytes_at: byte index {i} = {n}, must be in 0..=255",
                            builder.name
                        ))
                        .with_span(span.clone()),
                    );
                    return builder.clone();
                }
                payload.push(n as u8);
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    img.raw_writes.push(RawWrite {
                        lba: lba as u64,
                        source: RawSource::Bytes { bytes: payload },
                        max_sectors: None,
                        span: Some(span),
                    });
                }
            }
            builder.clone()
        },
    );

    // Cap on the most recently appended raw_write. We deliberately
    // attach to the *last* entry rather than threading a per-write
    // builder type — chains stay flat (`write_crate_at(...).max_sectors(64)`)
    // and there's no incentive to apply a cap to anything but the
    // write you just declared.
    engine.register_fn(
        "max_sectors",
        |builder: &mut ImageBuilder, n: i64| -> ImageBuilder {
            if builder.is_duplicate {
                return builder.clone();
            }
            if n <= 0 {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").max_sectors: must be positive, got {n}",
                        builder.name
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            let mut model = builder.state.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    if let Some(last) = img.raw_writes.last_mut() {
                        last.max_sectors = Some(n as u64);
                    } else {
                        builder.state.push_diagnostic(
                            Diagnostic::error(format!(
                                "image(\"{}\").max_sectors called with no preceding write_*_at(...)",
                                builder.name
                            ))
                            .with_span(builder.state.span_from(builder.pos)),
                        );
                    }
                }
            }
            builder.clone()
        },
    );

    // -----------------------------------------------------------------
    // Sub-partition builder.
    //
    // image(...).partition("esp", 2048, "fat32")
    //     .partition_type(0xEF)
    //     .bootable(true)
    //     .add_crate("kernel", "/boot/kernel.bin")
    //     .end()
    //
    // The opening call appends a `PartitionDef` and returns a
    // PartitionBuilder pinned to that index. `.end()` returns an
    // `ImageBuilder` for the same image so the caller can keep chaining
    // at the disk level.
    // -----------------------------------------------------------------

    let s_part = state.clone();
    engine.register_fn(
        "partition",
        move |ctx: NativeCallContext,
              builder: &mut ImageBuilder,
              name: &str,
              start_lba: i64,
              fs_type: &str|
              -> PartitionBuilder {
            let pos = ctx.call_position();
            let span = s_part.span_from(pos);
            if builder.is_duplicate {
                return PartitionBuilder {
                    state: s_part.clone(),
                    image_name: builder.name.clone(),
                    partition_index: usize::MAX,
                    pos,
                    is_duplicate: true,
                };
            }
            if name.is_empty() {
                s_part.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").partition(...): name must not be empty",
                        builder.name
                    ))
                    .with_span(span.clone()),
                );
            }
            if start_lba < 0 {
                s_part.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").partition(\"{name}\", {start_lba}, ...): start_lba must be >= 0",
                        builder.name
                    ))
                    .with_span(span.clone()),
                );
            }
            let mut idx = usize::MAX;
            let mut model = s_part.model.borrow_mut();
            if let Some(h) = model.images.lookup(&builder.name) {
                if let Some(img) = model.images.get_mut(h) {
                    // Reject duplicate partition names on the same image
                    // — the name is what the user uses in diagnostics
                    // and (later) in `${partition:image:name}` lookups.
                    if img.partitions.iter().any(|p| p.name == name) {
                        s_part.push_diagnostic(
                            Diagnostic::error(format!(
                                "image(\"{}\").partition(\"{name}\", ...): a partition with this name already exists on this image",
                                builder.name
                            ))
                            .with_span(span.clone()),
                        );
                    } else {
                        idx = img.partitions.len();
                        img.partitions.push(PartitionDef {
                            name: name.into(),
                            start_lba: start_lba.max(0) as u64,
                            end_lba: None,
                            fs_type: fs_type.into(),
                            // Default to 0x83 (Linux) — a safe non-zero
                            // value. Users typically override via
                            // `.partition_type(0xEF)` for the ESP.
                            partition_type: 0x83,
                            bootable: false,
                            entries: Vec::new(),
                            span: Some(span),
                        });
                    }
                }
            }
            PartitionBuilder {
                state: s_part.clone(),
                image_name: builder.name.clone(),
                partition_index: idx,
                pos,
                is_duplicate: idx == usize::MAX,
            }
        },
    );

    register_partition_methods(engine, state.clone());
}

fn register_partition_methods(engine: &mut Engine, state: EngineState) {
    // Helper closure to mutate the targeted partition. Returns whether
    // a mutation happened (i.e. handle resolved + index in bounds).
    fn with_partition_mut(builder: &PartitionBuilder, f: impl FnOnce(&mut PartitionDef)) {
        if builder.is_duplicate || builder.partition_index == usize::MAX {
            return;
        }
        let mut model = builder.state.model.borrow_mut();
        if let Some(h) = model.images.lookup(&builder.image_name) {
            if let Some(img) = model.images.get_mut(h) {
                if let Some(p) = img.partitions.get_mut(builder.partition_index) {
                    f(p);
                }
            }
        }
    }

    engine.register_fn(
        "end_lba",
        |builder: &mut PartitionBuilder, lba: i64| -> PartitionBuilder {
            if lba < 0 {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").partition: end_lba must be >= 0, got {lba}",
                        builder.image_name
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            with_partition_mut(builder, |p| p.end_lba = Some(lba as u64));
            builder.clone()
        },
    );

    engine.register_fn(
        "partition_type",
        |builder: &mut PartitionBuilder, ty: i64| -> PartitionBuilder {
            if !(0..=0xFF).contains(&ty) {
                builder.state.push_diagnostic(
                    Diagnostic::error(format!(
                        "image(\"{}\").partition: partition_type must fit in a byte (0..=255), got {ty}",
                        builder.image_name
                    ))
                    .with_span(builder.state.span_from(builder.pos)),
                );
                return builder.clone();
            }
            with_partition_mut(builder, |p| p.partition_type = ty as u8);
            builder.clone()
        },
    );

    engine.register_fn(
        "bootable",
        |builder: &mut PartitionBuilder, flag: bool| -> PartitionBuilder {
            with_partition_mut(builder, |p| p.bootable = flag);
            builder.clone()
        },
    );

    engine.register_fn(
        "add_crate",
        |builder: &mut PartitionBuilder, crate_name: &str, dest: &str| -> PartitionBuilder {
            with_partition_mut(builder, |p| {
                p.entries.push(ImageEntry {
                    source: ImageSource::Crate(crate_name.into()),
                    dest_path: dest.into(),
                });
            });
            builder.clone()
        },
    );

    engine.register_fn(
        "add_file",
        |builder: &mut PartitionBuilder, path: &str, dest: &str| -> PartitionBuilder {
            with_partition_mut(builder, |p| {
                p.entries.push(ImageEntry {
                    source: ImageSource::File(path.into()),
                    dest_path: dest.into(),
                });
            });
            builder.clone()
        },
    );

    engine.register_fn(
        "add_esp",
        |builder: &mut PartitionBuilder, esp_name: &str, dest: &str| -> PartitionBuilder {
            with_partition_mut(builder, |p| {
                p.entries.push(ImageEntry {
                    source: ImageSource::Esp(esp_name.into()),
                    dest_path: dest.into(),
                });
            });
            builder.clone()
        },
    );

    // Close the partition sub-builder and hand the caller back an
    // ImageBuilder for the same image, so they can keep chaining at
    // the disk level (declare another partition, set `size`, etc.).
    let s_end = state.clone();
    engine.register_fn(
        "end",
        move |builder: &mut PartitionBuilder| -> ImageBuilder {
            ImageBuilder {
                state: s_end.clone(),
                name: builder.image_name.clone(),
                pos: builder.pos,
                is_duplicate: builder.is_duplicate,
            }
        },
    );
}
