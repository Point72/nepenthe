//! The `nepenthe` command-line interface.
//!
//! A single multicall binary: installed as `nepenthe` with `np` and `npb`
//! symlinks. All logic lives in [`nepenthe_core::cli`]; the invoked name
//! (argv[0]) selects the behaviour (`npb` → `nepenthe build`).

use std::process::ExitCode;

fn main() -> ExitCode {
    nepenthe_core::cli::run_multicall()
}
