use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nepenthe_core::backend::SpecStore;
use nepenthe_core::manifest::{Manifest, Overrides, Selector};
use nepenthe_core::registry::{Coordinates, Label, Registry};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/e2e")
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nonce}"))
}

#[test]
fn e2e_yaml_manifest_and_registry_workflow() {
    let fixtures = fixture_dir();
    let manifest_path = fixtures.join("manifest.yaml");
    let overrides_path = fixtures.join("overrides.yaml");

    // 1) Load manifest from file and merge imported YAML fragments.
    let mut manifest = Manifest::load(&manifest_path).expect("manifest should load from yaml");
    let overrides = Overrides::from_yaml_path(&overrides_path).expect("overrides should load");

    // 2) Apply overrides and resolve concrete build cells.
    manifest.apply(&overrides);
    let targets = manifest.targets("app").expect("targets should resolve");
    assert_eq!(targets.len(), 4);

    let default_cell = manifest
        .resolve_default("app")
        .expect("default cell should resolve");
    assert_eq!(default_cell.variant.as_deref(), Some("cpu"));
    assert!(default_cell
        .dependencies
        .contains(&"numpy >=2,<2.2".to_string()));
    assert!(default_cell
        .constraints
        .contains(&"pytorch * cpu*".to_string()));

    let gpu_cell = manifest
        .resolve(
            "app",
            &Selector {
                variant: Some("gpu".into()),
                python: Some("3.12".into()),
            },
        )
        .expect("gpu cell should resolve");
    assert_eq!(gpu_cell.variant.as_deref(), Some("gpu"));
    assert_eq!(gpu_cell.python.as_deref(), Some("3.12"));
    assert!(gpu_cell
        .constraints
        .contains(&"pytorch * cuda*".to_string()));

    // 3) Publish resolved lock bytes into a local registry and resolve labels.
    let root = unique_temp_dir("nepenthe-e2e-yaml-registry");
    fs::create_dir_all(&root).expect("temp registry dir should be creatable");

    let registry = Registry::new(SpecStore::new(), format!("file://{}", root.display()));
    let coords = Coordinates::new("app", "linux-64")
        .with_python("3.11")
        .with_variant("cpu");

    registry
        .publish(&coords, "1.0.0", b"lock-v1")
        .expect("first publish should succeed");
    registry
        .publish(&coords, "1.1.0", b"lock-v2")
        .expect("second publish should succeed");

    let latest = registry
        .resolve(&coords, &Label::Latest)
        .expect("latest should resolve");
    assert_eq!(latest.version, "1.1.0");

    let latest_lock = registry
        .pull(&coords, &Label::Latest)
        .expect("latest lock should pull");
    assert_eq!(latest_lock, b"lock-v2");

    let older_lock = registry
        .pull(&coords, &Label::LatestButOne)
        .expect("latest-but-one lock should pull");
    assert_eq!(older_lock, b"lock-v1");
}
