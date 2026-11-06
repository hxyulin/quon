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

QEMU-only options do not apply to a command runner: `--gdb`, `--uefi`,
`--direct` and `--bios` fail on a profile that uses one. `--timeout`
and the profile's `test_timeout` apply to every runner.
