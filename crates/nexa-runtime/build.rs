use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use nexa_machine::MachineSpec;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo provides CARGO_MANIFEST_DIR"),
    );
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("runtime crate is under workspace/crates");
    let specs = workspace.join("specs/machines");
    println!("cargo:rerun-if-changed={}", specs.display());

    let mut paths = std::fs::read_dir(&specs)
        .expect("machine spec directory exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "spec")
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut generated = String::new();
    for path in paths {
        let spec = MachineSpec::from_path(&path).unwrap_or_else(|errors| {
            let rendered = errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            panic!("invalid machine spec {}:\n{rendered}", path.display());
        });
        writeln!(generated, "{}", spec.generate_rust()).expect("writing String cannot fail");
    }

    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"))
        .join("machines.rs");
    std::fs::write(output, generated).expect("write generated state machines");
}
