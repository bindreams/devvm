use devvm::exit::ExitWidth;
use devvm_cli::{Deps, main_entry};

/// Placeholder: `build` never returns a guest code, so the width is unused until the CLI's `Host` supplies it.
const DEFAULT_EXIT_WIDTH: ExitWidth = ExitWidth::LowByte;

fn main() {
    let args = std::env::args_os().skip(1).collect();
    std::process::exit(main_entry(DEFAULT_EXIT_WIDTH, args, Deps::real()));
}
