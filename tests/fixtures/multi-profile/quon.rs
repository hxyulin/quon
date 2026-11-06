// Multi-profile fixture exercising features not covered by the
// minimal fixture:
//   - declared profiles (debug + release) with their own settings
//   - default_profile
//   - cfg flags on a crate
//   - a dependency with features
//   - a custom JSON target spec

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("multiprofile_test", "0.1.0");
    q.default_target(&Target::spec(
        "x86_64-unknown-test",
        "targets/x86_64-unknown-test.json",
    ));

    // The integration test checks both profiles' settings and that
    // `debug` is the default.
    let debug = q.add(Profile::new("debug").opt_level(0).debug_info(true));
    q.default_profile(&debug);
    q.add(
        Profile::new("release")
            .opt_level(2)
            .debug_info(false)
            .lto(Lto::Thin),
    );

    let mylib = q.add(Crate::lib("mylib", "crates/mylib").edition(Edition::E2021));
    q.add(
        Crate::bin("app", "crates/app")
            .edition(Edition::E2021)
            .cfg("my_feature")
            .cfg("test_mode")
            .dep(Dep::on(&mylib).features(["logging"])),
    );

    Ok(())
}
