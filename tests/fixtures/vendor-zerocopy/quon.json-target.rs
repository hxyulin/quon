// `quon.rs` with a custom JSON target instead of the builtin triple.
//
// zerocopy has a build script that runs for the target unit.
// zerocopy-derive is a proc macro, so it and the syn stack it uses
// (proc-macro2 and quote have build scripts) are host units.
// Network-gated: `cargo vendor` fetches from crates.io.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("vendor-zerocopy", "0.1.0");

    // The test writes this spec from `rustc --print target-spec-json`.
    let kernel = Target::spec("x86_64-quon-test", "targets/x86_64-quon-test.json");
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
