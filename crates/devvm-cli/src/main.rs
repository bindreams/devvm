use devvm::host_exit_width;
use devvm_cli::{Deps, run};

fn main() {
    let args = std::env::args_os().skip(1).collect();
    run(host_exit_width(), args, Deps::real());
}
