fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    assert_eq!(std::env::var("DEP_PROBE_KEY").as_deref(), Ok("42"));
}
