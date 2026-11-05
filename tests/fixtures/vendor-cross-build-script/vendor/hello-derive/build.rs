// No rerun-if directive: any file in the package reruns this script.
fn main() {
    println!("cargo:rustc-check-cfg=cfg(hello_bd)");
    println!("cargo:rustc-cfg={}", bd_helper::cfg_name());
}
