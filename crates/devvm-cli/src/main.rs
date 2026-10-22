use devvm::exit::ExitWidth;
use devvm_cli::{Deps, main_entry};

/// How much of a guest's exit code this host passes on. A placeholder: the CLI's `Host` supplies the real answer
/// (`Host::exit_width()`) and this constant goes away with it. Only `build`, which never returns a guest code, exists
/// until then.
const DEFAULT_EXIT_WIDTH: ExitWidth = ExitWidth::LowByte;

fn main() {
    let args = std::env::args_os().skip(1).collect();
    std::process::exit(main_entry(DEFAULT_EXIT_WIDTH, args, Deps::real()));
}
