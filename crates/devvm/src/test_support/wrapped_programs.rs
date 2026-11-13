//! The programs a wrapped command runs in the tests of the Windows guest's wrapper: the test binary in a helper mode
//! (`test_support::helper`), which the script finds by its full path or under a name a test gives it.

use std::io::Write;

/// An argument or a path as hex of its UTF-8 bytes: nothing the console or a pipe does to text can change it.
pub fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

/// Helper `wrap-argv`: prints each argument as `ARG<n>=<hex>`, then `CWD=<hex>`.
pub fn argv_echo(args: &[String]) {
    let mut out = std::io::stdout().lock();
    for (index, arg) in args.iter().enumerate() {
        writeln!(out, "ARG{index}={}", hex(arg)).unwrap();
    }
    writeln!(
        out,
        "CWD={}",
        hex(&std::env::current_dir().unwrap().display().to_string())
    )
    .unwrap();
}

/// Helper `wrap-exit`: ends with the 32 bits given as a decimal.
pub fn exit_with(args: &[String]) {
    let code: u32 = args[0].parse().unwrap();
    std::process::exit(code as i32);
}

/// Helper `wrap-bytes`: every byte value to stdout and the same reversed to stderr (`all256`), or text with every line
/// ending and no final newline (`crlf`).
pub fn write_bytes(args: &[String]) {
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());
    match args[0].as_str() {
        "all256" => {
            let up: Vec<u8> = (0..=255).collect();
            let down: Vec<u8> = (0..=255).rev().collect();
            out.write_all(&up).unwrap();
            err.write_all(&down).unwrap();
        }
        "crlf" => {
            out.write_all(b"a\r\nb\nc\r\n\r\nlast").unwrap();
            err.write_all(b"e1\r\ne2\n").unwrap();
        }
        other => panic!("wrap-bytes does not know `{other}`"),
    }
}
