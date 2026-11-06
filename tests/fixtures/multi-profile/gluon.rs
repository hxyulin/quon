// Multi-profile fixture exercising features not covered by the
// minimal fixture:
//   - Multiple profiles (debug + release) with different settings
//   - default_profile
//   - cfg_flags on a crate
//   - Dependency with features
//   - Two groups (host + cross)

use gluon_config::{Config, CrateType, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("multiprofile_test", "0.1.0")
        .default_profile("debug");

    let target = cfg.target_spec("x86_64-unknown-test", "targets/x86_64-unknown-test.json");

    cfg.profile("debug")
        .target(target)
        .opt_level(0)
        .debug_info(true);

    cfg.profile("release")
        .target(target)
        .opt_level(2)
        .debug_info(false)
        .lto("thin");

    cfg.group("host_tools")
        .target_host()
        .edition("2021");

    // Both kernel-group crates land via the same bound GroupBuilder
    // so the second `.add` doesn't trigger Duplicate state.
    let mut cross = cfg.group("kernel").target(target).edition("2021");
    cross.add("mylib", "crates/mylib");
    cross.add("app", "crates/app")
        .crate_type(CrateType::Bin)
        .cfg_flag("my_feature")
        .cfg_flag("test_mode")
        .dep_with("mylib", "mylib", |b| {
            b.features(&["logging"]);
        });
    drop(cross);

    Ok(())
}
