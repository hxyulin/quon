// Host-tool fixture: a profile that builds for the host, declares no
// target, and runs its binary directly with `gluon run`.

use gluon_config::{Config, Result};

pub fn config(cfg: &mut Config) -> Result<()> {
    cfg.project("host-tool", "0.1.0");
    cfg.cargo_workspace(".");
    cfg.group("tools").target_host().package("hello");
    cfg.profile("dev")
        .target_host()
        .opt_level(0)
        .debug_info(true)
        .run_binary("hello");
    Ok(())
}
