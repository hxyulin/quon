//! `gluon-runner-qemu`: the QEMU runner as a plugin executable. Gluon
//! has the same runner built in; this binary serves it over the runner
//! protocol, for tools that call runners the way Gluon calls external
//! ones.

fn main() -> std::process::ExitCode {
    gluon_runner::serve(&gluon_runner_qemu::QemuRunner)
}
