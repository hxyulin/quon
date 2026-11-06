# Pipelines and Rules

Compilation is only part of a bare-metal build. After compiling, you often
need to copy binaries, strip symbols, create disk images, or assemble
boot partitions. Quon handles these post-processing steps through
**rules** and **pipelines**.

## Rules

A rule is a custom build step with inputs, outputs, and a handler that
defines what the step does. Rules participate in the build DAG just like
crate compilations -- they respect dependency ordering and are subject to
caching.

### Built-in rule types

Quon provides three built-in handlers:

**copy** -- copy a file from one location to another:

```rust
cfg.rule("copy-kernel")
    .handler("copy")
    .inputs(&["${build_dir}/kernel"])
    .output("${build_dir}/boot/kernel.elf");
```

**exec** -- run an arbitrary command. The command and its arguments
are passed through the rule's input/output substitution machinery:

```rust
cfg.rule("strip-kernel")
    .handler("exec")
    .inputs(&["${build_dir}/kernel"])
    .output("${build_dir}/kernel.bin");
```

**mkimage** -- create disk images (the exact behavior is
implementation-dependent and may vary by target and boot protocol).

### Variable substitution

Rule arguments support variable substitution using `${...}` syntax. The
following variables are available:

| Variable       | Description                                      |
|----------------|--------------------------------------------------|
| `${build_dir}` | The build output directory for the current target and profile |
| `${target}`    | The target triple                                |
| `${profile}`   | The active profile name                          |
| `${input}`     | The rule's input path (resolved)                 |
| `${output}`    | The rule's output path (resolved)                |

Variables are resolved at execution time, so they always reflect the
current build context.
