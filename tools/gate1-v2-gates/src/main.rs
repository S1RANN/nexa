use std::path::Path;

use nexa_gate1_v2_4::{AnyError, repository_root};

fn main() -> Result<(), AnyError> {
    std::env::set_current_dir(repository_root())?;
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [command] if command == "all" => nexa_gate1_v2_4_gates::package_all(),
        [command, raw, output] if command == "regenerate" => {
            nexa_gate1_v2_4_gates::generate_from_raw(Path::new(raw), Path::new(output))
        }
        _ => Err(
            "usage: nexa-gate1-v2-4-gates all|regenerate <raw-directory> <output-directory>".into(),
        ),
    }
}
