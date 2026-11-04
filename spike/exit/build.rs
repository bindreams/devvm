fn main() {
    let rustc = std::env::var("RUSTC").unwrap();
    let out = std::process::Command::new(rustc).arg("-V").output().unwrap();
    println!(
        "cargo:rustc-env=RUSTC_VERSION={}",
        String::from_utf8(out.stdout).unwrap().trim()
    );
}
