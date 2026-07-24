use std::path::Path;

use nexa_machine::{MachineSpec, transition_id_map};

fn main() {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().unwrap_or_else(|| "check".into());
    let path = arguments.next().unwrap_or_else(|| "specs/machines".into());
    let result = match command.as_str() {
        "check" => check(Path::new(&path)),
        "generate" => generate(Path::new(&path)),
        _ => Err(format!(
            "unknown command `{command}`; expected `check [path]` or `generate <spec>`"
        )),
    };
    if let Err(error) = result {
        eprintln!("nexa-machine: {error}");
        std::process::exit(1);
    }
}

fn check(path: &Path) -> Result<(), String> {
    let specs = load_specs(path)?;
    let mut transition_count = 0;
    for (path, spec) in &specs {
        transition_count += transition_id_map(spec)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len();
    }
    println!(
        "validated {} machine specifications with {transition_count} transitions",
        specs.len()
    );
    Ok(())
}

fn generate(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        return Err("generate expects one machine spec file".into());
    }
    let spec = MachineSpec::from_path(path).map_err(|errors| {
        errors
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    print!("{}", spec.generate_rust());
    Ok(())
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
