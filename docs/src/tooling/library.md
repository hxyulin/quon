# Quon as a library

A repository's own tooling, such as an `xtask`, can drive Quon through
the `quon` crate instead of a `quon` binary on `$PATH`. The Cargo lock
then pins the Quon version the tool runs, so a fresh clone needs no
separate install:

```toml
[dependencies]
quon = { git = "https://github.com/hxyulin/quon", rev = "<commit>" }
```

The library has two parts, and nothing else of Quon is public API.

## Running commands

`quon::run(dir, args)` runs the command line in-process, as
`quon <args>` would from `dir`, and returns the status the binary would
exit with:

```rust,ignore
use std::process::ExitCode;

let code = quon::run(&root, ["build", "chassis", "--release"]);
if code != ExitCode::SUCCESS {
    return Err("quon build failed".into());
}
```

- `args` leaves out the program name.
- Output and errors go to stdout and stderr, as the binary prints
  them.
- `dir` stands in for the current directory: Quon searches upward
  from it for `quon.rs`, and relative paths in `args` resolve against
  it. The process's own current directory is neither read nor changed.
- `run` never ends the process. A usage error returns status 2, and
  `--help` returns success.

`quon run` handles SIGINT and SIGTERM while its runner is up, so it can
stop the runner cleanly. Its first use registers handlers that give
both signals their default action outside a run. Handlers the host
program registered itself for those signals no longer apply after
that.

## Reading the project

`quon::Project` evaluates `quon.rs` once and reports what it declares,
without building anything:

```rust,ignore
let project = quon::Project::load(&root)?;
for entry in project.entries() {
    let elf = project.artifact(entry, "release")?;
    println!("{:?} -> {}", entry.name, elf.display());
}
```

| Item | Meaning |
|------|---------|
| `Project::root()` | The directory holding `quon.rs` |
| `Project::entries()` | The `q.run(..)` entries, in declaration order |
| `Project::entry(name)` | The entry with that `.name(..)` |
| `Entry::name` | The entry's name; `None` only for a project's single unnamed entry |
| `Entry::package` | The `Package` (or `Crate`) declaration the binary belongs to |
| `Entry::binary` | The binary crate `quon run` starts |
| `Entry::target` | The name of the target the entry builds for |
| `Project::artifact(entry, profile)` | Where `quon build <entry> -p <profile>` writes the binary |

`artifact` uses the same code as `quon run` to find the file, so a tool
that flashes or attaches to the binary never repeats Quon's output
layout. It reads `.quon-config` and `QUON_*` overrides as `quon` does.
The file may be missing or stale, since `artifact` builds nothing; run
`quon::run(.., ["build", ..])` first.
