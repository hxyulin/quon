pub fn base() -> u32 {
    if cfg!(feature = "extra") { 41 } else { 0 }
}
