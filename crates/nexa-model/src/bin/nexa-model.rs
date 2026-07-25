use std::path::{Path, PathBuf};

use nexa_machine::MachineSpec;
use nexa_model::explore;
use nexa_model::realm_v3::{RealmV3Config, explore_realm_v3};
use nexa_model::realm_v4::{RealmV4Config, explore_realm_v4, explore_realm_v4_routing};
use nexa_model::system::{
    RealmSystemConfig, SystemConfig, explore_realm_runtime, explore_task_scope,
};

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

#[allow(clippy::too_many_lines)]
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
    if path.is_dir() {
        let config = SystemConfig::parse(include_str!(
            "../../../../specs/systems/task_scope.system.spec"
        ))?;
        let report = explore_task_scope(config);
        if !report.failures.is_empty() {
            return Err(format!(
                "TaskScope system model failed: {:?}",
                report.failures
            ));
        }
        println!(
            "TaskScope: {} worlds, {} rejected operations",
            report.visited_worlds, report.rejected_operations
        );
        let realm = explore_realm_runtime(RealmSystemConfig::parse(include_str!(
            "../../../../specs/systems/realm_runtime.system.spec"
        ))?);
        if !realm.failures.is_empty() {
            return Err(format!(
                "RealmRuntime system model failed: {:?}",
                realm.failures
            ));
        }
        println!(
            "RealmRuntime: {} worlds, {} rejected operations",
            realm.visited_worlds, realm.rejected_operations
        );
        let realm_v3 = explore_realm_v3(RealmV3Config {
            max_depth: 14,
            max_worlds: 4_096,
        });
        if !realm_v3.failures.is_empty() || realm_v3.truncated {
            return Err(format!(
                "Realm v3 model failed: {:?}, truncated={}",
                realm_v3.failures, realm_v3.truncated
            ));
        }
        println!(
            "RealmV3: {} worlds, {} rejected operations",
            realm_v3.visited_worlds, realm_v3.rejected_operations
        );
        let realm_v4 = explore_realm_v4(RealmV4Config {
            max_depth: 16,
            max_worlds: 4_096,
        });
        if !realm_v4.failures.is_empty() || realm_v4.truncated {
            return Err(format!(
                "Realm v4 model failed: {:?}, truncated={}",
                realm_v4.failures, realm_v4.truncated
            ));
        }
        println!(
            "RealmV4: {} worlds, {} task states, {} rejected operations",
            realm_v4.visited_worlds,
            realm_v4.reached_states.len(),
            realm_v4.rejected_operations
        );
        let realm_v4_routing = explore_realm_v4_routing(RealmV4Config {
            max_depth: 8,
            max_worlds: 256,
        });
        if !realm_v4_routing.failures.is_empty() || realm_v4_routing.truncated {
            return Err(format!(
                "Realm v4 routing model failed: {:?}, truncated={}",
                realm_v4_routing.failures, realm_v4_routing.truncated
            ));
        }
        println!(
            "RealmV4 routing: {} worlds, {} rejected operations",
            realm_v4_routing.visited_worlds, realm_v4_routing.rejected_operations
        );
    }
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
