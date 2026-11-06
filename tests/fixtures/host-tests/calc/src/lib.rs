#![cfg_attr(not(test), no_std)]

pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds() {
        assert_eq!(super::add(2, 2), helper::four());
    }

    // Host tests run from the package directory, as under `cargo test`.
    #[test]
    fn runs_in_the_package_directory() {
        let manifest = std::fs::read_to_string("Cargo.toml").unwrap();
        assert!(manifest.contains("name = \"calc\""));
        assert_eq!(env!("CARGO_PKG_NAME"), "calc");
    }

    // Fails when CALC_FAIL is set, so the CLI tests can see a failure.
    #[test]
    fn fails_on_request() {
        assert!(std::env::var_os("CALC_FAIL").is_none(), "CALC_FAIL is set");
    }
}
