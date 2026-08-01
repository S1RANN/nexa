use std::path::Path;

use nexa_machine::{MachineSpec, generate_rust_modules, stable_id_map};

const DEFAULT_SPECS: &str = "specs/machines";
const DEFAULT_GENERATED: &str = "crates/nexa-runtime/src/generated/machines.rs";

fn main() {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "check".into());
    let path = arguments.next().unwrap_or_else(|| DEFAULT_SPECS.into());
    let output = arguments.next().unwrap_or_else(|| DEFAULT_GENERATED.into());
    if arguments.next().is_some() {
        fail("too many arguments");
    }
    let result = match command.as_str() {
        "check" => check(Path::new(&path)),
        "generate" => generate(Path::new(&path), Path::new(&output)),
        "check-generated" => check_generated(Path::new(&path), Path::new(&output)),
        _ => Err(usage(&format!("unknown command `{command}`"))),
    };
    if let Err(error) = result {
        fail(&error);
    }
}

fn fail(error: &str) -> ! {
    eprintln!("nexa-machine: {error}");
    std::process::exit(1);
}

fn usage(message: &str) -> String {
    format!(
        "{message}; expected `check [spec-path]`, `generate [spec-path] [output]`, or \
         `check-generated [spec-path] [output]`"
    )
}

fn check(path: &Path) -> Result<(), String> {
    let specs = load_specs(path)?;
    let mut transition_count = 0;
    for (path, spec) in &specs {
        transition_count += spec.transitions.len();
        stable_id_map(spec).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    println!(
        "validated {} machine specifications; {transition_count} transitions",
        specs.len()
    );
    Ok(())
}

fn generate(path: &Path, output: &Path) -> Result<(), String> {
    let generated = generate_source(path)?;
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    std::fs::write(output, generated)
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    println!("generated {}", output.display());
    Ok(())
}

fn check_generated(path: &Path, output: &Path) -> Result<(), String> {
    let expected = generate_source(path)?;
    let actual = std::fs::read_to_string(output)
        .map_err(|error| format!("could not read {}: {error}", output.display()))?;
    if actual != expected {
        return Err(format!(
            "{} is stale; run `cargo run -p nexa-machine -- generate`",
            output.display()
        ));
    }
    println!("generated machine code is current: {}", output.display());
    Ok(())
}

fn generate_source(path: &Path) -> Result<String, String> {
    let specs = load_specs(path)?;
    Ok(generate_rust_modules(specs.iter().map(|(_, spec)| spec)))
}

fn load_specs(path: &Path) -> Result<Vec<(std::path::PathBuf, MachineSpec)>, String> {
    let mut paths = if path.is_dir() {
        std::fs::read_dir(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|entry| {
                entry
                    .extension()
                    .is_some_and(|extension| extension == "spec")
            })
            .collect::<Vec<_>>()
    } else {
        vec![path.to_path_buf()]
    };
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no `.spec` files found under {}", path.display()));
    }
    paths
        .into_iter()
        .map(|path| {
            MachineSpec::from_path(&path)
                .map(|spec| (path.clone(), spec))
                .map_err(|errors| {
                    errors
                        .into_iter()
                        .map(|error| format!("{}: {error}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
        })
        .collect()
}
