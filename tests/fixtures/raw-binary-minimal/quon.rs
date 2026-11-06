// Minimal fixture for the RawBinary ELF-sidecar integration test.
//
// Declares one project, one builtin target and one
// RawBinary crate with a linker script. The linker script intentionally
// omits OUTPUT_FORMAT — Task 9 covers the negative case where it's
// present.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("raw-binary-minimal", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    q.add(
        Crate::raw_binary("boot", "crates/boot")
            .edition(Edition::E2021)
            .linker_script("crates/boot/boot.ld"),
    );

    Ok(())
}
