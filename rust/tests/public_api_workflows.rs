use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use nepenthe_core::backend::SpecStore;
use nepenthe_core::manifest::{Manifest, Overrides, Selector};
use nepenthe_core::registry::{Coordinates, Label, Registry};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nonce}"))
}

#[test]
fn public_manifest_workflow_without_internal_fixtures() {
    let manifest_yaml = r#"
project:
  name: demo
  channels: [conda-forge]
  platforms: [linux-64]
  python: ["3.11", "3.12"]
  default-python: "3.11"

dependencies:
  - numpy >=2

features:
  dev:
    dependencies: [pytest]

variants:
  cpu: {}
  gpu:
    dependencies: [cuda >=12]
    constraints: ["pytorch * cuda*"]

environments:
  app:
    features: [dev]
    variants: [cpu, gpu]
    default-variant: cpu
"#;

    let overrides_yaml = r#"
pins:
  numpy: ">=2,<2.2"

variants:
  cpu:
    constraints: ["pytorch * cpu*"]
"#;

    let mut manifest = Manifest::from_yaml_str(manifest_yaml).expect("manifest should parse");
    let overrides = Overrides::from_yaml_str(overrides_yaml).expect("overrides should parse");
    manifest.apply(&overrides);

    let targets = manifest.targets("app").expect("targets should resolve");
    assert_eq!(targets.len(), 4);

    let default_cell = manifest
        .resolve_default("app")
        .expect("default cell should resolve");
    assert_eq!(default_cell.variant.as_deref(), Some("cpu"));
    assert!(
        default_cell
            .dependencies
            .contains(&"numpy >=2,<2.2".to_string()),
        "pins should be baked into dependency specs"
    );
    assert!(
        default_cell
            .constraints
            .contains(&"pytorch * cpu*".to_string()),
        "cpu constraints should come from overrides"
    );

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
        .dependencies
        .iter()
        .any(|d| d.starts_with("python 3.12")));
    assert!(gpu_cell
        .constraints
        .contains(&"pytorch * cuda*".to_string()));
}

#[test]
fn public_registry_workflow_with_local_backend() {
    let root = unique_temp_dir("nepenthe-public-registry-test");
    fs::create_dir_all(&root).expect("temp registry dir should be creatable");

    let root_url = format!("file://{}", root.display());
    let registry = Registry::new(SpecStore::new(), root_url);
    let coords = Coordinates::new("demo", "linux-64")
        .with_python("3.11")
        .with_variant("cpu");

    registry
        .publish(&coords, "1.0.0", b"lock v1")
        .expect("first publish should succeed");
    registry
        .publish(&coords, "1.1.0", b"lock v2")
        .expect("second publish should succeed");

    let latest = registry
        .resolve(&coords, &Label::Latest)
        .expect("latest should resolve");
    assert_eq!(latest.version, "1.1.0");

    let latest_lock = registry
        .pull(&coords, &Label::Latest)
        .expect("latest lock should pull");
    assert_eq!(latest_lock, b"lock v2");

    let previous_lock = registry
        .pull(&coords, &Label::LatestButOne)
        .expect("latest-but-one lock should pull");
    assert_eq!(previous_lock, b"lock v1");

    let duplicate = registry
        .publish(&coords, "1.1.0", b"lock v2")
        .expect("idempotent publish should return existing release");
    assert_eq!(duplicate.version, "1.1.0");

    let tampered = registry.publish(&coords, "1.1.0", b"tampered lock bytes");
    assert!(
        tampered.is_err(),
        "publishing same version with different bytes must fail"
    );
}
