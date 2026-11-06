// Fixture for the negative RawBinary integration test.
//
// Identical to raw-binary-minimal except the project name and the
// linker script, which contains OUTPUT_FORMAT(binary) — a directive
// that collides with quon's post-link objcopy step and must produce
// a specific diagnostic pointing the user at the offending directive.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("raw-binary-bad-linker", "0.1.0");
    q.default_target(&Target::builtin("x86_64-unknown-none"));

    q.add(
        Crate::raw_binary("boot", "crates/boot")
            .edition(Edition::E2021)
            .linker_script("crates/boot/boot.ld"),
    );

    Ok(())
}
