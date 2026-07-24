use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nexa_machine::{MachineSpec, transition_id_map};
use nexa_model::explore;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match arguments.as_slice() {
        [area, command] if area == "baseline" && command == "check" => check_baseline(),
        [area, command] if area == "machine" && command == "check" => check_machines(),
        [area, command] if area == "model" && command == "check" => check_models(),
        _ => Err("usage: nexa baseline check | nexa machine check | nexa model check".to_owned()),
    };
    if let Err(error) = result {
        eprintln!("nexa: {error}");
        std::process::exit(1);
    }
}

fn check_baseline() -> Result<(), String> {
    const REQUIRED: &[&str] = &[
        "baseline/BASELINE_INDEX.md",
        "baseline/mvr/MVR_SCOPE.md",
        "baseline/mvr/MVR_NON_GOALS.md",
        "baseline/runtime/TASK_MACHINE.md",
        "baseline/runtime/SCOPE_MACHINE.md",
        "baseline/runtime/MODULE_MACHINE.md",
        "baseline/runtime/HOST_REQUEST_MACHINE.md",
        "baseline/runtime/RESOURCE_MACHINE.md",
        "baseline/runtime/HANDLES.md",
        "baseline/reload/RELOAD_TRANSACTION.md",
        "baseline/abi/BYTECODE.md",
        "baseline/abi/IDL.md",
        "baseline/abi/RUST_HOST_ABI.md",
        "baseline/testing/EXPERIMENT_PROTOCOL.md",
        "baseline/testing/GATE0_BENCHMARKS.md",
        "baseline/testing/GATE0_KILL_CRITERIA.md",
    ];
    for required in REQUIRED {
        if !Path::new(required).is_file() {
            return Err(format!("required normative file `{required}` is missing"));
        }
    }

    let index = std::fs::read_to_string(REQUIRED[0])
        .map_err(|error| format!("could not read baseline index: {error}"))?;
    let mut decision_ids = BTreeSet::new();
    for line in index.lines().filter(|line| line.starts_with("| D")) {
        let id = line
            .split('|')
            .nth(1)
            .map(str::trim)
            .ok_or_else(|| format!("malformed decision row `{line}`"))?;
        if !decision_ids.insert(id.to_owned()) {
            return Err(format!("decision `{id}` appears more than once"));
        }
    }
    if decision_ids.is_empty() {
        return Err("baseline index contains no active decisions".into());
    }

    for path in REQUIRED {
        let contents = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read `{path}`: {error}"))?;
        if contents.contains("rationale/history/") {
            return Err(format!(
                "normative file `{path}` refers to historical rationale as a source"
            ));
        }
    }
    println!(
        "baseline snapshot is complete: {} files, {} indexed decisions",
        REQUIRED.len(),
        decision_ids.len()
    );
    Ok(())
}

fn check_machines() -> Result<(), String> {
    let specs = load_specs()?;
    let transition_count = specs
        .iter()
        .map(|(_, spec)| {
            transition_id_map(spec)
                .map(|map| map.len())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum::<usize>();
    println!(
        "machine specifications are valid: {} machines, {transition_count} transitions",
        specs.len()
    );
    Ok(())
}

fn check_models() -> Result<(), String> {
    let specs = load_specs()?;
    let mut snapshot_count = 0;
    for (_, spec) in &specs {
        let report = explore(spec);
        if !report.is_success(spec) {
            return Err(format!(
                "model `{}` failed:\n{}",
                spec.name,
                report
                    .failures
                    .iter()
                    .map(|failure| failure.message.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        snapshot_count += report.visited_snapshots;
    }
    println!(
        "bounded model exploration passed: {} machines, {snapshot_count} snapshots",
        specs.len()
    );
    Ok(())
}

fn load_specs() -> Result<Vec<(PathBuf, MachineSpec)>, String> {
    let directory = Path::new("specs/machines");
    let mut paths = std::fs::read_dir(directory)
        .map_err(|error| format!("could not read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| {
            entry
                .extension()
                .is_some_and(|extension| extension == "spec")
        })
        .collect::<Vec<_>>();
    paths.sort();
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
