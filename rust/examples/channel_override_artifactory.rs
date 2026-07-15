use std::path::PathBuf;

use nepenthe_core::manifest::Manifest;
use nepenthe_core::solve::ChannelSettings;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/artifactory")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1) Load a manifest whose channels are overridden to point at an internal
    //    Artifactory (via `channel-alias` + a per-channel `mirror`).
    let manifest = Manifest::load(fixture_dir().join("manifest.yaml"))?;
    let settings = ChannelSettings::from_manifest(&manifest);

    println!("channel-alias => {:?}", settings.channel_alias);
    for channel in &manifest.project.channels {
        // Each declared channel resolves to its effective Artifactory URL.
        println!("{channel:>18} => {}", settings.resolve(channel));
    }

    // 2) The same override can be built programmatically (no manifest needed).
    let programmatic =
        ChannelSettings::with_alias("https://artifactory.mycompany.net/artifactory/api/conda")
            .mirror("conda-forge", "conda-forge-mirror");
    println!(
        "programmatic conda-forge => {}",
        programmatic.resolve("conda-forge")
    );

    Ok(())
}
