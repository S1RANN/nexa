use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nexa_machine::{MachineSpec, stable_id_map};
use nexa_model::explore;
use nexa_model::realm_v3::{RealmV3Config, explore_realm_v3};
use nexa_model::realm_v4::{RealmV4Config, explore_realm_v4};
use nexa_model::system::{
    RealmSystemConfig, SystemConfig, explore_realm_runtime, explore_task_scope,
};

const REQUIRED_BASELINE: &[&str] = &[
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

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match arguments.as_slice() {
        [area, command] if area == "baseline" && command == "check" => check_baseline(),
        [area, command] if area == "machine" && command == "check" => check_machines(),
        [area, command] if area == "model" && command == "check" => check_models(),
        [command, path] if command == "compile" => compile_file(Path::new(path)),
        [area, command, path] if area == "idl" && command == "check" => check_idl(Path::new(path)),
        [area, command, path] if area == "idl" && command == "generate" => {
            generate_idl(Path::new(path))
        }
        _ => Err(
            "usage: nexa baseline check | nexa machine check | nexa model check | \
             nexa compile <file> | nexa idl check|generate <file>"
                .to_owned(),
        ),
    };
    if let Err(error) = result {
        eprintln!("nexa: {error}");
        std::process::exit(1);
    }
}

fn compile_file(path: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let module = nexa_compiler::compile(&source).map_err(|error| error.to_string())?;
    println!(
        "compiled and verified {}: {} functions",
        path.display(),
        module.module().functions.len()
    );
    Ok(())
}

fn check_idl(path: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let idl = nexa_idl::parse(&source).map_err(|error| error.to_string())?;
    println!(
        "IDL {} is valid; exact hash {}",
        path.display(),
        nexa_idl::exact_hash(&idl)
    );
    Ok(())
}

fn generate_idl(path: &Path) -> Result<(), String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let idl = nexa_idl::parse(&source).map_err(|error| error.to_string())?;
    print!("{}", nexa_idl::generate_rust(&idl));
    Ok(())
}

fn check_baseline() -> Result<(), String> {
    for required in REQUIRED_BASELINE {
        if !Path::new(required).is_file() {
            return Err(format!("required normative file `{required}` is missing"));
        }
    }

    let index = std::fs::read_to_string(REQUIRED_BASELINE[0])
        .map_err(|error| format!("could not read baseline index: {error}"))?;
    let normative_files = files_with_extension(Path::new("baseline"), "md")?;
    check_decision_index(&index)?;
    check_normative_files(&index, &normative_files)?;
    check_deferred_api_leaks(Path::new("crates"))?;
    check_toolchain_consistency()?;

    let decision_count = table_rows(&index, "## Active decisions").len();
    println!(
        "baseline snapshot is complete: {} files, {decision_count} active decisions",
        normative_files.len()
    );
    Ok(())
}

fn check_decision_index(index: &str) -> Result<(), String> {
    let mut decision_ids = BTreeSet::new();
    let active = table_rows(index, "## Active decisions");
    if active.is_empty() {
        return Err("baseline index contains no active decisions".into());
    }
    for row in &active {
        if row.len() != 5 {
            return Err(format!(
                "malformed active decision row `{}`",
                row.join(" | ")
            ));
        }
        let id = &row[0];
        if !is_decision_id(id) {
            return Err(format!("invalid decision ID `{id}`"));
        }
        if row[1] != "Active" {
            return Err(format!(
                "decision `{id}` has invalid active status `{}`",
                row[1]
            ));
        }
        if !decision_ids.insert(id.clone()) {
            return Err(format!("decision `{id}` appears more than once"));
        }
        let location = row[3].trim_matches('`');
        let normative_path = if location == "this file" {
            PathBuf::from(REQUIRED_BASELINE[0])
        } else {
            Path::new("baseline").join(location)
        };
        if !normative_path.is_file() {
            return Err(format!(
                "active decision `{id}` refers to missing normative path `{}`",
                normative_path.display()
            ));
        }
    }

    for row in table_rows(index, "## Deferred decisions") {
        if row.len() != 3 || row[1] != "Deferred" {
            return Err(format!(
                "malformed deferred decision row `{}`",
                row.join(" | ")
            ));
        }
    }
    for row in table_rows(index, "## Superseded decisions") {
        if row.len() != 3 || row[1] != "Superseded" {
            return Err(format!(
                "malformed superseded decision row `{}`",
                row.join(" | ")
            ));
        }
    }
    Ok(())
}

fn check_normative_files(index: &str, normative_files: &[PathBuf]) -> Result<(), String> {
    let superseded = table_rows(index, "## Superseded decisions")
        .into_iter()
        .map(|row| row[0].to_lowercase())
        .collect::<Vec<_>>();
    for path in normative_files {
        let contents = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
        if !has_version_declaration(&contents) {
            return Err(format!(
                "normative file `{}` has no version declaration",
                path.display()
            ));
        }
        if contents.contains("rationale/history/") {
            return Err(format!(
                "normative file `{}` refers to historical rationale as a source",
                path.display()
            ));
        }
        if path != Path::new(REQUIRED_BASELINE[0]) {
            let normalized = contents.to_lowercase();
            if let Some(item) = superseded
                .iter()
                .find(|item| normalized.contains(item.as_str()))
            {
                return Err(format!(
                    "normative file `{}` refers to superseded item `{item}`",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn check_deferred_api_leaks(root: &Path) -> Result<(), String> {
    const DEFERRED_IDENTIFIERS: &[&str] = &[
        "dynamic",
        "dynamicvalue",
        "usergeneric",
        "userdefinedgeneric",
        "crossmodule",
        "reloadgroup",
        "readlease",
        "writelease",
        "compatibleabiadapter",
        "strictdeterminism",
        "untrustedbytecode",
        "securityverifier",
        "aot",
        "jit",
    ];
    for path in files_with_extension(root, "rs")? {
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| format!("could not read `{}`: {error}", path.display()))?;
        for (line_index, source_line) in contents.lines().enumerate() {
            let code = strip_string_literals(source_line.split("//").next().unwrap_or_default());
            for token in
                code.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            {
                let normalized = token.replace('_', "").to_ascii_lowercase();
                if DEFERRED_IDENTIFIERS.contains(&normalized.as_str()) {
                    return Err(format!(
                        "deferred identifier `{token}` appears in {}:{}",
                        path.display(),
                        line_index + 1
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_toolchain_consistency() -> Result<(), String> {
    let toolchain = std::fs::read_to_string("rust-toolchain.toml")
        .map_err(|error| format!("could not read rust-toolchain.toml: {error}"))?;
    let channel = toolchain
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("channel")
                .and_then(|value| value.split_once('='))
                .map(|(_, value)| value.trim().trim_matches('"'))
        })
        .ok_or("rust-toolchain.toml has no channel")?;
    let readme = std::fs::read_to_string("README.md")
        .map_err(|error| format!("could not read README.md: {error}"))?;
    if !readme.contains(channel) {
        return Err(format!(
            "README.md does not mention pinned Rust toolchain `{channel}`"
        ));
    }
    Ok(())
}

fn strip_string_literals(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            output.push(' ');
        } else if character == '"' {
            in_string = true;
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    output
}

fn files_with_extension(root: &Path, expected_extension: &str) -> Result<Vec<PathBuf>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("could not read {}: {error}", directory.display()))?
        {
            let path = entry
                .map_err(|error| format!("could not inspect {}: {error}", directory.display()))?
                .path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == expected_extension)
            {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn table_rows(contents: &str, heading: &str) -> Vec<Vec<String>> {
    let mut in_section = false;
    let mut rows = Vec::new();
    for line in contents.lines() {
        if line == heading {
            in_section = true;
            continue;
        }
        if in_section && line.starts_with("## ") {
            break;
        }
        if in_section && line.starts_with('|') {
            let cells = line
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if !cells.is_empty()
                && !cells[0].starts_with("---")
                && !matches!(cells[0].as_str(), "ID" | "Capability" | "Item")
            {
                rows.push(cells);
            }
        }
    }
    rows
}

fn is_decision_id(id: &str) -> bool {
    id.strip_prefix('D').is_some_and(|number| {
        !number.is_empty() && number.chars().all(|digit| digit.is_ascii_digit())
    })
}

fn has_version_declaration(contents: &str) -> bool {
    contents.lines().take(6).any(|line| {
        line.contains("Version:")
            || line.split_whitespace().any(|token| {
                let token = token.trim_matches(|character: char| {
                    !character.is_ascii_digit() && character != '.'
                });
                token.contains('.')
                    && token.split('.').all(|part| {
                        !part.is_empty() && part.chars().all(|digit| digit.is_ascii_digit())
                    })
            })
    })
}

fn check_machines() -> Result<(), String> {
    let specs = load_specs()?;
    let mut global_ids = std::collections::BTreeMap::new();
    let mut transition_count = 0;
    for (path, spec) in &specs {
        transition_count += spec.transitions.len();
        for (id, name) in
            stable_id_map(spec).map_err(|error| format!("{}: {error}", path.display()))?
        {
            if let Some(existing) = global_ids.insert(id, name.clone()) {
                return Err(format!(
                    "global stable ID collision between `{existing}` and `{name}`"
                ));
            }
        }
    }
    println!(
        "machine specifications are valid: {} machines, {transition_count} transitions, {} stable IDs",
        specs.len(),
        global_ids.len()
    );
    Ok(())
}

fn check_models() -> Result<(), String> {
    let artifact = Path::new("target/model-artifacts/shortest-failure.txt");
    if let Some(parent) = artifact.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create model artifact directory: {error}"))?;
    }
    let specs = load_specs()?;
    let mut snapshot_count = 0;
    for (_, spec) in &specs {
        let report = explore(spec);
        if !report.is_success(spec) {
            let failure = report.failures.first().map_or_else(
                || "coverage failure without transition path\n".to_owned(),
                |failure| format!("{}\npath={:?}\n", failure.message, failure.path),
            );
            std::fs::write(artifact, failure)
                .map_err(|error| format!("could not write model failure artifact: {error}"))?;
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
    let system_config = SystemConfig::parse(include_str!(
        "../../../specs/systems/task_scope.system.spec"
    ))?;
    let system_report = explore_task_scope(system_config);
    if !system_report.failures.is_empty() {
        let (message, path) = &system_report.failures[0];
        std::fs::write(artifact, format!("{message}\npath={path:?}\n"))
            .map_err(|error| format!("could not write model failure artifact: {error}"))?;
        return Err(format!(
            "TaskScope system model failed: {:?}",
            system_report.failures
        ));
    }
    let realm_config = RealmSystemConfig::parse(include_str!(
        "../../../specs/systems/realm_runtime.system.spec"
    ))?;
    let realm_report = explore_realm_runtime(realm_config);
    if !realm_report.failures.is_empty() {
        return Err(format!(
            "RealmRuntime system model failed: {:?}",
            realm_report.failures
        ));
    }
    let realm_v3 = explore_realm_v3(RealmV3Config {
        max_depth: 14,
        max_worlds: 4_096,
    });
    if !realm_v3.failures.is_empty() {
        let (message, path) = &realm_v3.failures[0];
        std::fs::write(artifact, format!("{message}\npath={path:?}\n"))
            .map_err(|error| format!("could not write model artifact: {error}"))?;
        return Err(format!("Realm v3 model failed: {:?}", realm_v3.failures));
    }
    if realm_v3.truncated {
        return Err("Realm v3 model exploration was truncated".into());
    }
    let realm_v4 = explore_realm_v4(RealmV4Config {
        max_depth: 16,
        max_worlds: 4_096,
    });
    if !realm_v4.failures.is_empty() {
        let (message, path) = &realm_v4.failures[0];
        std::fs::write(artifact, format!("{message}\npath={path:?}\n"))
            .map_err(|error| format!("could not write model artifact: {error}"))?;
        return Err(format!("Realm v4 model failed: {:?}", realm_v4.failures));
    }
    if realm_v4.truncated {
        return Err("Realm v4 model exploration was truncated".into());
    }
    std::fs::write(
        artifact,
        format!(
            "success: {} machine snapshots, {} task/scope worlds, {} realm worlds, {} realm-v3 worlds, {} realm-v4 worlds\n",
            snapshot_count,
            system_report.visited_worlds,
            realm_report.visited_worlds,
            realm_v3.visited_worlds,
            realm_v4.visited_worlds,
        ),
    )
    .map_err(|error| format!("could not write model artifact: {error}"))?;
    println!(
        "bounded model exploration passed: {} machines, {snapshot_count} snapshots, {} task/scope worlds, {} realm worlds, {} realm-v3 worlds, {} realm-v4 worlds",
        specs.len(),
        system_report.visited_worlds,
        realm_report.visited_worlds,
        realm_v3.visited_worlds,
        realm_v4.visited_worlds,
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
