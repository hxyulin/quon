//! `quon-runner-qemu`: the QEMU runner as a plugin executable. Quon
//! has the same runner built in; this binary serves it over the runner
//! protocol, for tools that call runners the way Quon calls external
//! ones.

fn main() -> std::process::ExitCode {
    quon_runner::serve(&quon_runner_qemu::QemuRunner)
}
