# Runners

A runner says how Quon starts a built binary. `quon run` starts the
binary `q.run` names with it, and `quon test` runs the integration
tests and boot tests with it. Host tests always run on the host.

Declare a runner with `q.add`, which returns a `Runner` handle, and
name it in the run entry:

```rust,ignore
let qemu = q.add(Qemu::new("qemu-system-x86_64").machine("q35").serial(Serial::Stdio));
q.run(&kernel).runner(&qemu);

// Inherits the `qemu` runner from `dev`.
let dev = q.profile("dev");
q.add(Profile::new("test").inherits(&dev));
```

Every profile uses the run entry's runner unless it, or a profile it
inherits from, names another with `Profile::runner(&r)`. A profile with
no runner can still build, but `quon run` and the integration tests
fail with `profile '<name>' has no runner`. The exception is a
[host profile](../configuration/projects-targets-profiles.md#host-profiles),
which runs its binaries directly when it names no runner.

Runners have no name in `quon.rs`. Quon names them after their kind
(`qemu`, `qemu-2`, `command`, or the plugin's name), and those names
appear only in messages.

## Kinds

| Kind | Declared with | What it does |
|------|---------------|--------------|
| QEMU | `Qemu::new(program)` | Boots the binary under QEMU. See [QEMU Orchestration](./qemu.md). |
| Command | `Command::new([..])` | Runs the argv, with each `{binary}` replaced by the binary's path. |
| Plugin | `Plugin::new(name)` | Asks the runner plugin `name` for the command. See [Runner plugins](#runner-plugins). |

## Command runners

A command runner covers anything that is not QEMU: flashing a board,
another emulator, or a script of your own.

```rust,ignore
let board = q.add(Command::new(["probe-rs", "run", "--chip", "nRF52840_xxAA", "{binary}"]));

let dev = q.profile("dev");
q.add(Profile::new("hw").inherits(&dev).runner(&board));
```

`quon -p hw run -- --speed 4000` builds the profile and runs
`probe-rs run --chip nRF52840_xxAA <path to the kernel> --speed 4000`.
The arguments after `--` are appended to the argv.

The run passes when the command exits with status 0. A command that
reports success another way can name its own status:

```rust,ignore
let board = q.add(Command::new(["./scripts/run-on-board.sh", "{binary}"]).success_exit_code(33));
```

With `.success_exit_code(n)` set, every other status, 0 included, is a
failure.

`--gdb`, `--uefi`, `--direct` and `--bios` need a runner plugin, such
as QEMU, and fail on a profile that uses a command runner. `--timeout`
applies to every runner.

## Runner plugins

A runner plugin turns a built binary and its settings into the command
Quon starts. Quon keeps the build, the per-test ESP copies, timeouts
and spawning. QEMU is a runner plugin built into the `quon` binary:
`Qemu::new(program)` declares a runner served by the `qemu` plugin, with
the QEMU builder's values as its settings.

Any other plugin is an executable named `quon-runner-<name>` on
`$PATH`. Declare a runner that uses one with `Plugin::new(name)` and set
its settings with `.setting(key, value)`. The value is a string, a
number or a bool, or any JSON value built with
`quon_config::serde_json::json!`.

```rust,ignore
let board = q.add(Plugin::new("probe-rs").setting("chip", "nRF52840_xxAA").setting("speed", 4000));

let dev = q.profile("dev");
q.add(Profile::new("hw").inherits(&dev).runner(&board));
```

To offer a typed builder for a plugin's settings, implement `Declare`;
see [Your own declarations](../configuration/quon-rs.md#your-own-declarations).

`quon -p hw run` then starts `quon-runner-probe-rs` twice:

1. Before the build, with a `validate` request. The plugin checks its
   settings. Its reply says how `quon test` swaps an integration
   test's binary in for the run binary, and may give a default test
   timeout.
2. After the build, with a `prepare` request: the binary's path, the
   paths of the project's ESPs, images and build steps, the arguments
   after `--`, and whether `--gdb` was given. The reply is the program,
   its arguments and environment, the exit status that counts as
   success, and notes for Quon to print before the run.

Each call writes one JSON request to the plugin's stdin and reads one
JSON reply from its stdout. The plugin's stderr goes to the terminal.
`--uefi`, `--direct` and `--bios` reach the plugin as a `boot_mode`
override of its settings.

The `quon-runner` crate has the request and reply types and a
`serve` function that does the reading and writing, so a plugin's
`main` is one line:

```rust,ignore
fn main() -> std::process::ExitCode {
    quon_runner::serve(&MyRunner)
}
```

The `quon-runner-qemu` crate builds the QEMU runner as such an
executable too, and serves as an example of the `Runner` trait.
