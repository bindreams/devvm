//! Exports the compile target triple, which `crate::TARGET` and the QEMU manifest lookup read.

fn main() {
    let target = std::env::var("TARGET").expect("cargo sets TARGET for build scripts");
    println!("cargo:rustc-env=DEVVM_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
