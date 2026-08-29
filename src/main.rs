//! Command-line entry point.
//!
//! This is the bootstrap skeleton: it establishes the crate, the lint floor, and the
//! toolchain that CI verifies, and deliberately implements no archive or image handling.
//! The pipeline arrives with the `zip-jpeg-vertical` slice.

use clap::Parser;

/// Auto-resize the pages of a comic archive and repack them as zip.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {}

fn main() {
    let Cli {} = Cli::parse();
    println!(
        "{} {} — not implemented yet; see README.md",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
    );
}
