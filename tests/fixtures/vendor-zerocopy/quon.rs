// zerocopy 0.8 with `derive`, vendored for a kernel target.
//
// zerocopy has a build script that runs for the target unit.
// zerocopy-derive is a proc macro, so it and the syn stack it uses
// (proc-macro2 and quote have build scripts) are host units.
// Network-gated: `cargo vendor` fetches from crates.io. The test swaps in
// `quon.json-target.rs` to cover a custom JSON target.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-zerocopy", "0.1.0");

    let kernel = Target::builtin("x86_64-unknown-none");
    q.default_target(&kernel);

    let zerocopy = q.add(
        Dependency::crates_io("zerocopy", "0.8")
            .features(["derive"])
            .target(&kernel),
    );

    q.add(
        Crate::lib("kernel", "crates/kernel")
            .edition(Edition::E2021)
            .dep(&zerocopy),
    );

    Ok(())
}
