use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    assert_eq!(env::var("DEP_PROBE_KEY").as_deref(), Ok("42"), "links metadata");
    assert_eq!(env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("none"));
    assert_eq!(env::var("TARGET").as_deref(), Ok("x86_64-unknown-none"));
    println!("cargo:rustc-check-cfg=cfg(consumer_ok)");
    println!("cargo:rustc-cfg=consumer_ok");
}
