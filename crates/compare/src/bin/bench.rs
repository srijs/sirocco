//! The benchmark suite without a base revision; `scripts/bench.sh` builds
//! one with it. See `crates/compare/BENCH.md`.
//!
//! ```sh
//! cargo run -p compare --release --bin bench -- --help
//! ```

fn main() {
    compare::suite::main(Vec::new());
}
