use nepenthe_core::manifest::{Manifest, Overrides, Selector};

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    dependencies: [pytest, ruff]

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

    let mut manifest = Manifest::from_yaml_str(manifest_yaml)?;
    let overrides = Overrides::from_yaml_str(overrides_yaml)?;
    manifest.apply(&overrides);

    let default_cell = manifest.resolve_default("app")?;
    println!("default app deps: {:?}", default_cell.dependencies);

    let gpu_cell = manifest.resolve(
        "app",
        &Selector {
            variant: Some("gpu".into()),
            python: Some("3.12".into()),
        },
    )?;
    println!("gpu app constraints: {:?}", gpu_cell.constraints);

    Ok(())
}
