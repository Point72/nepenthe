//! Regenerate the vendored PyPI→conda name mapping (`src/data/pypi_to_conda.tsv`).
//!
//! Fetches conda-forge's grayskull PyPI→conda mapping and reduces it to the
//! divergent pairs nepenthe needs (see [`nepenthe_core::name_map`]). Run with
//! network access:
//!
//! ```bash
//! cargo run --example regenerate_name_map
//! ```
//!
//! The output is deterministic (sorted), so a regenerated table diffs cleanly
//! against the committed one — commit the result if it changed.

use std::path::PathBuf;

use nepenthe_core::name_map::{reduce_grayskull, GRAYSKULL_URL};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/data/pypi_to_conda.tsv");

    eprintln!("fetching {GRAYSKULL_URL}");
    let yaml = reqwest::blocking::Client::builder()
        .user_agent(concat!("nepenthe/", env!("CARGO_PKG_VERSION")))
        .build()?
        .get(GRAYSKULL_URL)
        .send()?
        .error_for_status()?
        .text()?;
    eprintln!("fetched {} bytes", yaml.len());

    let reduced = reduce_grayskull(&yaml)?;
    let count = reduced.lines().count();

    std::fs::write(&dest, reduced.as_bytes())?;
    eprintln!(
        "wrote {count} divergent mappings ({} bytes) to {}",
        reduced.len(),
        dest.display()
    );
    Ok(())
}
