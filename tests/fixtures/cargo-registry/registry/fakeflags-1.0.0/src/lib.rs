#![no_std]

pub const ANSWER: u32 = 42;

// Warns without `--cap-lints allow`; Quon passes it for registry
// packages, as Cargo does.
pub fn unused_variable() {
    let x = 1;
}
