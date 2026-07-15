use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use nepenthe_core::backend::SpecStore;
use nepenthe_core::registry::{Coordinates, Label, Registry};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root_dir = std::env::temp_dir().join(format!("nepenthe-example-registry-{nonce}"));
    fs::create_dir_all(&root_dir)?;

    let root_url = format!("file://{}", root_dir.display());
    let registry = Registry::new(SpecStore::new(), root_url);

    let coords = Coordinates::new("demo", "linux-64").with_python("3.11");

    registry.publish(&coords, "1.0.0", b"demo-lock-v1")?;
    registry.publish(&coords, "1.1.0", b"demo-lock-v2")?;

    let latest = registry.resolve(&coords, &Label::Latest)?;
    let lock_bytes = registry.pull(&coords, &Label::Latest)?;

    println!("latest version: {}", latest.version);
    println!("latest lock bytes: {}", String::from_utf8(lock_bytes)?);

    Ok(())
}
