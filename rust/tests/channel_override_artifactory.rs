use std::path::PathBuf;

use nepenthe_core::manifest::Manifest;
use nepenthe_core::solve::ChannelSettings;

const ALIAS: &str = "https://artifactory.mycompany.net/artifactory/api/conda";

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/artifactory")
}

#[test]
fn channels_resolve_to_artifactory_from_yaml() {
    let manifest = Manifest::load(fixture_dir().join("manifest.yaml"))
        .expect("manifest should load from yaml");
    let settings = ChannelSettings::from_manifest(&manifest);

    // The alias carried over from the manifest's project.channel-alias.
    assert_eq!(settings.channel_alias.as_deref(), Some(ALIAS));

    // conda-forge keeps its public identity but is mirrored to the internal
    // conda-forge-mirror, which then resolves against the Artifactory alias.
    assert_eq!(
        settings.resolve("conda-forge"),
        format!("{ALIAS}/conda-forge-mirror")
    );

    // my-custom-channel is a bare name with no entry, so it resolves directly
    // against the Artifactory alias.
    assert_eq!(
        settings.resolve("my-custom-channel"),
        format!("{ALIAS}/my-custom-channel")
    );

    // Every declared channel ends up under the Artifactory host.
    for channel in &manifest.project.channels {
        assert!(
            settings.resolve(channel).starts_with(ALIAS),
            "channel '{channel}' did not resolve under Artifactory: {}",
            settings.resolve(channel)
        );
    }
}

#[test]
fn channels_resolve_to_artifactory_via_builder() {
    // The same override expressed programmatically, without a manifest.
    let settings = ChannelSettings::with_alias(ALIAS).mirror("conda-forge", "conda-forge-mirror");

    assert_eq!(
        settings.resolve("conda-forge"),
        format!("{ALIAS}/conda-forge-mirror")
    );
    assert_eq!(
        settings.resolve("my-custom-channel"),
        format!("{ALIAS}/my-custom-channel")
    );
    // An already-resolved URL is returned unchanged.
    assert_eq!(
        settings.resolve("https://conda.anaconda.org/conda-forge"),
        "https://conda.anaconda.org/conda-forge"
    );
}
