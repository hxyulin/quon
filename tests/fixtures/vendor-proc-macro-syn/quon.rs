// A kernel crate that uses a syn-based proc macro from crates.io
// directly, as hadron's crates use their derive crates.
//
// bytemuck_derive is a proc macro, so it and the syn stack it uses
// (proc-macro2 and quote have build scripts) are host units; the kernel
// gets only `--extern bytemuck_derive=<host dylib>`.
// Network-gated: `cargo vendor` fetches from crates.io.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-proc-macro-syn", "0.1.0");

    let kernel = Target::builtin("x86_64-unknown-none");
    q.default_target(&kernel);

    let bytemuck = q.add(Dependency::crates_io("bytemuck", "1").target(&kernel));
    let bytemuck_derive = q.add(Dependency::crates_io("bytemuck_derive", "1").target(&kernel));

    q.add(
        Crate::lib("kernel", "crates/kernel")
            .edition(Edition::E2021)
            .dep(&bytemuck)
            .dep(&bytemuck_derive),
    );

    Ok(())
}
