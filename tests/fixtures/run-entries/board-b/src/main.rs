fn main() {
    // Both boards use `common` the same way.
    assert!(!common::GREETING.is_empty());
    println!("{}", chip::NAME);
}
