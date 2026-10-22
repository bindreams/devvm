use devvm::exit::ExitWidth;
use devvm_cli::{Deps, run};

/// Placeholder: `build` never returns a guest code, so the width is unused until the CLI's `Host` supplies it.
const DEFAULT_EXIT_WIDTH: ExitWidth = ExitWidth::LowByte;

fn main() {
    let args = std::env::args_os().skip(1).collect();
    run(DEFAULT_EXIT_WIDTH, args, Deps::real());
}
