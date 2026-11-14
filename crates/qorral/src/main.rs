//! `qorral`: the command line. Every GUI action has a command here; scripts
//! and other applications use `--json` and the exit codes (`qorral help`).

fn main() {
    std::process::exit(qorral_app::cli::run(std::env::args().skip(1).collect()));
}
