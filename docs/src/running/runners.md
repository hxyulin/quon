# Runners

A runner says how Gluon starts a built binary. `gluon run` starts the
profile's run binary with it, and `gluon test` runs the integration
tests and boot tests with it. Host tests always run on the host.

Declare a runner by name, then point a profile at it:

```rust,ignore
cfg.runner("qemu")
    .qemu("qemu-system-x86_64")
    .machine("q35")
    .serial_stdio();

cfg.profile("dev")
    .target(x86_64)
    .run_binary("kernel")
    .runner("qemu");

// Inherits the `qemu` runner from `dev`.
cfg.profile("test").inherits("dev");
```

A profile inherits its runner through `inherits`. A profile with no
runner can still build, but `gluon run` and the integration tests fail
with `profile '<name>' has no runner`. The exception is a
[host profile](../configuration/projects-targets-profiles.md#host-profiles),
which runs its binaries directly when it names no runner.

## Kinds

| Kind | Declared with | What it does |
|------|---------------|--------------|
| QEMU | `.qemu(program)` | Boots the binary under QEMU. See [QEMU Orchestration](./qemu.md). |
| Command | `.command(&[...])` | Runs the argv, with each `{binary}` replaced by the binary's path. |
| Plugin | `.plugin(name)` | Asks the runner plugin `name` for the command. See [Runner plugins](#runner-plugins). |

## Command runners

A command runner covers anything that is not QEMU: flashing a board,
another emulator, or a script of your own.

```rust,ignore
cfg.runner("board")
    .command(&["probe-rs", "run", "--chip", "nRF52840_xxAA", "{binary}"]);

cfg.profile("hw").inherits("dev").runner("board");
```

`gluon -p hw run -- --speed 4000` builds the profile and runs
`probe-rs run --chip nRF52840_xxAA <path to the kernel> --speed 4000`.
The arguments after `--` are appended to the argv.

The run passes when the command exits with status 0. A command that
reports success another way can name its own status:

```rust,ignore
cfg.runner("board")
    .command(&["./scripts/run-on-board.sh", "{binary}"])
    .success_exit_code(33);
```

With `.success_exit_code(n)` set, every other status, 0 included, is a
failure.

`--gdb`, `--uefi`, `--direct` and `--bios` need a runner plugin, such
as QEMU, and fail on a profile that uses a command runner. `--timeout`
and the profile's `test_timeout` apply to every runner.

## Runner plugins

A runner plugin turns a built binary and its settings into the command
Gluon starts. Gluon keeps the build, the per-test ESP copies, timeouts
and spawning. QEMU is a runner plugin built into the `gluon` binary:
`.qemu(program)` declares a runner served by the `qemu` plugin, with
the QEMU builder's values as its settings.

Any other plugin is an executable named `gluon-runner-<name>` on
`$PATH`. Declare a runner that uses one with `.plugin(name)` and set
its settings with `.setting(key, value)`. The value is a string, a
number or a bool, or any JSON value built with
`gluon_config::serde_json::json!`.

```rust,ignore
cfg.runner("board")
    .plugin("probe-rs")
    .setting("chip", "nRF52840_xxAA")
    .setting("speed", 4000);

cfg.profile("hw").inherits("dev").runner("board");
```

`gluon -p hw run` then starts `gluon-runner-probe-rs` twice:

1. Before the build, with a `validate` request. The plugin checks its
   settings. Its reply says how `gluon test` swaps an integration
   test's binary in for the run binary, and may give a default test
   timeout.
2. After the build, with a `prepare` request: the binary's path, the
   paths of the project's ESPs, images and build steps, the arguments
   after `--`, and whether `--gdb` was given. The reply is the program,
   its arguments and environment, the exit status that counts as
   success, and notes for Gluon to print before the run.

Each call writes one JSON request to the plugin's stdin and reads one
JSON reply from its stdout. The plugin's stderr goes to the terminal.
`--uefi`, `--direct` and `--bios` reach the plugin as a `boot_mode`
override of its settings.

The `gluon-runner` crate has the request and reply types and a
`serve` function that does the reading and writing, so a plugin's
`main` is one line:

```rust,ignore
fn main() -> std::process::ExitCode {
    gluon_runner::serve(&MyRunner)
}
```

The `gluon-runner-qemu` crate builds the QEMU runner as such an
executable too, and serves as an example of the `Runner` trait.
