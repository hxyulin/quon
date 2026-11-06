// Outside-path fixture: `app` depends on `lib` in ../other, which
// inherits its version, edition, lints and its `helper` dependency
// with the `extra` feature from ../other/Cargo.toml. Builds for the
// host, so no sysroot is needed. `app` prints 42 only if `helper` got
// `extra`.

use quon_config::prelude::*;

pub fn config(q: &mut Quon) -> Result<()> {
    q.project("cargo-path-outside", "0.1.0");
    q.cargo_workspace(".");
    let app = q.add(Package::new("app"));
    q.run(&app);
    Ok(())
}
