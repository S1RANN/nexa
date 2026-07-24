use std::path::{Path, PathBuf};

use nexa_machine::MachineSpec;
use nexa_model::explore;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let first = arguments.next();
    let path = match first.as_deref() {
        None | Some("smoke") => "specs/machines".into(),
        Some(path) => path.to_owned(),
    };
    if arguments.next().is_some() {
        eprintln!("nexa-model: usage: nexa-model [smoke | <spec-or-directory>]");
        std::process::exit(1);
    }
    match run(Path::new(&path)) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("nexa-model: {error}");
            std::process::exit(1);
        }
    }
}

fn run(path: &Path) -> Result<(), String> {
    let paths = spec_paths(path)?;
    let mut snapshots = 0;
    let mut transitions = 0;
    for path in &paths {
        let spec = MachineSpec::from_path(path).map_err(|errors| {
            errors
                .into_iter()
                .map(|error| format!("{}: {error}", path.display()))
                .collect::<Vec<_>>()
                .join("\n")
        })?;
        let report = explore(&spec);
        if !report.is_success(&spec) {
            let failures = report
                .failures
                .iter()
                .map(|failure| {
                    if failure.path.is_empty() {
                        failure.message.clone()
                    } else {
                        format!("{} via {}", failure.message, failure.path.join(" → "))
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!("model `{}` failed:\n{failures}", spec.name));
        }
        snapshots += report.visited_snapshots;
        transitions += report.taken_transitions.len();
        println!(
            "{}: {} states, {} transitions, {} explored snapshots",
            spec.name,
            report.reachable_states.len(),
            report.taken_transitions.len(),
            report.visited_snapshots
        );
    }
    println!(
        "model exploration passed for {} machines, {transitions} transitions, {snapshots} snapshots",
        paths.len()
    );
    Ok(())
}

fn spec_paths(path: &Path) -> Result<Vec<PathBuf>, String> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut paths = std::fs::read_dir(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| {
            entry
                .extension()
                .is_some_and(|extension| extension == "spec")
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.is_empty() {
        Err(format!("no `.spec` files found under {}", path.display()))
    } else {
        Ok(paths)
    }
}
