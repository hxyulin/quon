# Build steps

Compilation is only part of a bare-metal build. After compiling, you
often need to strip symbols, convert a binary, or generate a file from
another one. A build step runs a host tool that Quon builds, and
writes one file.

```rust,ignore
let strip = q.add(Package::new("elf-strip").target(&Target::host()));
let kernel = q.add(Package::new("kernel"));

let stripped = q.add(
    BuildStep::new("kernel-stripped", "kernel.elf")
        .tool(&strip)
        .input("ELF", &kernel)
        .env("MODE", "all"),
);
```

`BuildStep::new(name, output)` declares a step that writes the file
`output`, a plain file name. Quon puts it under
`build-steps/<name>/` in the build directory.

| Method | Description |
|---|---|
| `.tool(&artifact)` | The binary that runs the step, a crate built for the host. Required |
| `.input(var, source)` | Set `var` to the path of `source`: an artifact, or a file relative to the project root |
| `.env(var, value)` | Set `var` to `value` in the tool's environment |

The tool runs with no arguments. It reads its inputs from the
environment and writes its output to the path in `QUON_OUTPUT_PATH`,
which no input or `env` entry may use as its name.

## When a step reruns

A build step is cached like a crate compile. It reruns when one of
these changes:

- the tool binary;
- an input artifact or input file;
- an `env` value or the output path.

## Using the output

`q.add` returns an `Artifact` handle for the step. Pass it where a
crate's output can go:

- `.artifact_env(var, &step)` on a package or crate, to embed the file
  at compile time;
- `.input(var, &step)` on another build step;
- `Image::add`, `Partition::add` and `Image::raw_write`, to put the
  file in a disk image.

Crates and build steps share one namespace, so a step cannot have the
name of a crate.
