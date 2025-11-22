use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=memory.x");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let target_script = out_dir.join("memory.x");
    fs::copy("memory.x", &target_script).expect("failed to copy memory.x");
    let link_script = out_dir.join("link.x");
    fs::copy("link.x", &link_script).expect("failed to copy link.x");
    println!("cargo:rustc-link-search={}", out_dir.display());
    println!("cargo:rustc-link-arg=-T{}", target_script.display());
    println!("cargo:rustc-link-arg=-T{}", link_script.display());
    println!("cargo:rerun-if-changed=build.rs");
}
