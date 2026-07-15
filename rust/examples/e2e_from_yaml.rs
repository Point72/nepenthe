use std::path::PathBuf;

use nepenthe_core::backend::SpecStore;
use nepenthe_core::manifest::{Manifest, Overrides, Selector};
use nepenthe_core::registry::{Coordinates, Label, Registry};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/e2e")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixtures = fixture_dir();

    let mut manifest = Manifest::load(fixtures.join("manifest.yaml"))?;
    let overrides = Overrides::from_yaml_path(fixtures.join("overrides.yaml"))?;
    manifest.apply(&overrides);

    let default_cell = manifest.resolve_default("app")?;
    println!(
        "default app => variant={:?}, python={:?}, deps={:?}",
        default_cell.variant, default_cell.python, default_cell.dependencies
    );

    let gpu_cell = manifest.resolve(
        "app",
        &Selector {
            variant: Some("gpu".into()),
            python: Some("3.12".into()),
        },
    )?;
    println!("gpu app constraints => {:?}", gpu_cell.constraints);

    let root = std::env::temp_dir().join("nepenthe-example-e2e-yaml-registry");
    let registry = Registry::new(SpecStore::new(), format!("file://{}", root.display()));
    let coords = Coordinates::new("app", "linux-64")
        .with_python("3.11")
        .with_variant("cpu");

    registry.publish(&coords, "1.0.0", b"lock-v1")?;
    registry.publish(&coords, "1.1.0", b"lock-v2")?;

    let latest = registry.resolve(&coords, &Label::Latest)?;
    println!("latest app lock version => {}", latest.version);

    Ok(())
}
