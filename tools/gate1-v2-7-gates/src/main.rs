use std::path::Path;

use nexa_gate1_v2_7::{AnyError, repository_root};

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [command] if command == "all" || command == "package-raw" => {
            nexa_gate1_v2_7_gates::package_all()
        }
        [command] if command == "rebuild-and-compare" => {
            nexa_gate1_v2_7_gates::rebuild_and_compare()
        }
        [command] if command == "artifact-hygiene" => nexa_gate1_v2_7_gates::artifact_hygiene(),
        [command, raw, output] if command == "regenerate" || command == "rebuild-and-compare" => {
            nexa_gate1_v2_7_gates::generate_from_raw(Path::new(raw), Path::new(output))
        }
        _ => Err(
            "usage: nexa-gate1-v2-7-gates package-raw|rebuild-and-compare|artifact-hygiene".into(),
        ),
    }
}
