use std::path::Path;

use nexa_gate1_v2_3_fixtures::{AnyError, FixtureCase, artifact_bundle};

fn main() -> Result<(), AnyError> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let [case, output] = arguments.as_slice() else {
        return Err("usage: nexa-gate1-v2-3-fixtures <case> <output>".into());
    };
    let fixture = artifact_bundle(FixtureCase::parse(case)?);
    let output = Path::new(output);
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, serde_json::to_vec_pretty(&fixture)?)?;
    println!("{}", output.display());
    Ok(())
}
