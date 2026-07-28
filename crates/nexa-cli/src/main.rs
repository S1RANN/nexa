use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nexa_machine::{MachineSpec, stable_id_map};
use nexa_model::artifact::{
    MODEL_FAILURE_ARTIFACT_VERSION, ModelFailureArtifact, current_commit_sha,
    write_model_failure_artifact,
};
use nexa_model::explore;
use nexa_model::realm::{RealmEvent, RealmModel};
use nexa_model::system::{
    RealmSystemConfig, SystemConfig, explore_realm_runtime, explore_task_scope,
};
use serde_json::{Value, json};

const REQUIRED_BASELINE: &[&str] = &[
    "baseline/BASELINE_INDEX.md",
    "baseline/internal/INTERNAL_LANGUAGE_SCOPE.md",
    "baseline/internal/HOST_BINDING.md",
    "baseline/internal/TASK_RUNTIME.md",
    "baseline/internal/RESTART_RELOAD.md",
    "baseline/runtime/TASK_MACHINE.md",
    "baseline/runtime/SCOPE_MACHINE.md",
    "baseline/runtime/MODULE_MACHINE.md",
    "baseline/runtime/HOST_REQUEST_MACHINE.md",
    "baseline/runtime/RESOURCE_MACHINE.md",
    "baseline/runtime/HANDLES.md",
    "baseline/abi/BYTECODE.md",
    "baseline/abi/IDL.md",
    "baseline/abi/RUST_HOST_ABI.md",
    "docs/TESTING.md",
    "docs/ARTIFACT_POLICY.md",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiagnosticFormat {
    Human,
    Json,
}

fn main() {
    let raw_arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let (diagnostic_format, arguments) = match extract_diagnostic_format(&raw_arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("nexa: {error}");
            std::process::exit(1);
        }
    };
    let result = match arguments.as_slice() {
        [command, arguments @ ..] if command == "check" => {
            check_command(arguments, diagnostic_format)
        }
        [command, arguments @ ..] if command == "build" => {
            build_command(arguments, diagnostic_format)
        }
        [command, arguments @ ..] if command == "verify" => {
            verify_command(arguments, diagnostic_format)
        }
        [command, arguments @ ..] if command == "run" => {
            run_command(arguments, diagnostic_format, false)
        }
        [command, arguments @ ..] if command == "trace" => {
            run_command(arguments, diagnostic_format, true)
        }
        [command] if command == "model-check" => check_models(),
        [command] if command == "diagnostic-corpus-check" => diagnostic_corpus_check(false),
        [command, flag, format]
            if command == "diagnostic-corpus-check" && flag == "--format" && format == "json" =>
        {
            diagnostic_corpus_check(true)
        }
        [command, path] if command == "model-replay" => model_replay(Path::new(path)),
        [command, arguments @ ..] if command == "fixture-check" => {
            fixture_check(arguments, diagnostic_format)
        }
        [area, command] if area == "baseline" && command == "check" => check_baseline(),
        [area, command] if area == "machine" && command == "check" => check_machines(),
        [area, command] if area == "model" && command == "check" => check_models(),
        [command, arguments @ ..] if command == "migrate-check" => migrate_check(arguments),
        [command, arguments @ ..] if command == "dump" => dump_module(arguments),
        [command, path] if command == "compile" => compile_file(Path::new(path)),
        [area, command, path] if area == "idl" && command == "check" => check_idl(Path::new(path)),
        [area, command, path] if area == "idl" && command == "generate" => {
            generate_idl(Path::new(path))
        }
        _ => Err("usage: nexa check|build|verify|dump|run|trace ... | \
             nexa model-check | nexa diagnostic-corpus-check | nexa model-replay <artifact.json> | \
             nexa migrate-check ... | nexa fixture-check <fixture-or-directory> | \
             nexa baseline check | nexa machine check | nexa idl check|generate <file> | \
             nexa migrate-check --old-module OLD --new-module NEW --state STATE \
             [--format human|json] [--output PATH] [--dump-state] [--diff-state] \
             [MigrationLimits] [--diagnostic-format human|json]"
            .to_owned()),
    };
    if let Err(error) = result {
        match diagnostic_format {
            DiagnosticFormat::Human => eprintln!("nexa: {error}"),
            DiagnosticFormat::Json => eprintln!(
                "{}",
                serde_json::to_string(&json!({
                    "status": "error",
                    "message": error,
                }))
                .expect("diagnostic JSON serialization does not fail")
            ),
        }
        std::process::exit(1);
    }
}

fn diagnostic_corpus_check(json_output: bool) -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let report = nexa::run_diagnostic_corpus(&root)?;
    if !report.missing_codes.is_empty()
        || !report.unexpected_codes.is_empty()
        || report.source_backed_inexact_spans != 0
        || !report.case_format.invalid_pipelines.is_empty()
    {
        return Err("diagnostic corpus contains failed cases".into());
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "diagnostic corpus: {} registered, {} observed, {} deterministic",
            report.registered_codes, report.observed_codes, report.deterministic_cases
        );
    }
    Ok(())
}

fn extract_diagnostic_format(
    arguments: &[String],
) -> Result<(DiagnosticFormat, Vec<String>), String> {
    let mut format = DiagnosticFormat::Human;
    let mut filtered = Vec::with_capacity(arguments.len());
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--diagnostic-format" {
            let value = arguments
                .get(index + 1)
                .ok_or("missing value for `--diagnostic-format`")?;
            format = match value.as_str() {
                "human" => DiagnosticFormat::Human,
                "json" => DiagnosticFormat::Json,
                _ => return Err("`--diagnostic-format` must be `human` or `json`".into()),
            };
            index += 2;
        } else {
            filtered.push(arguments[index].clone());
            index += 1;
        }
    }
    Ok((format, filtered))
}

fn check_command(arguments: &[String], format: DiagnosticFormat) -> Result<(), String> {
    let (path, limits) = parse_input_and_limits(arguments, "check")?;
    let verified = compile_source(&path)?;
    verify_with_limits(verified.module().clone(), limits)?;
    print_success(
        format,
        "check",
        &json!({
            "source": path,
            "functions": verified.module().functions.len(),
        }),
        &format!(
            "checked {}: {} functions",
            path.display(),
            verified.module().functions.len()
        ),
    );
    Ok(())
}

fn build_command(arguments: &[String], format: DiagnosticFormat) -> Result<(), String> {
    let mut source = None;
    let mut output = None;
    let mut limits_file = None;
    let mut dump_source_map = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    arguments
                        .get(index + 1)
                        .ok_or("missing value for build output")?,
                ));
                index += 2;
            }
            "--limits-file" => {
                limits_file = Some(PathBuf::from(
                    arguments
                        .get(index + 1)
                        .ok_or("missing value for `--limits-file`")?,
                ));
                index += 2;
            }
            "--dump-source-map" => {
                dump_source_map = true;
                index += 1;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown build option `{option}`"));
            }
            path if source.is_none() => {
                source = Some(PathBuf::from(path));
                index += 1;
            }
            path => return Err(format!("unexpected build argument `{path}`")),
        }
    }
    let source = source.ok_or("usage: nexa build <source.nexa> [-o module.nxb]")?;
    let verified = compile_source(&source)?;
    let limits = load_verifier_limits(limits_file.as_deref())?;
    verify_with_limits(verified.module().clone(), limits)?;
    let output = output.unwrap_or_else(|| source.with_extension("nxb"));
    std::fs::write(&output, verified.module().encode())
        .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    if dump_source_map {
        let mut rendered = String::new();
        render_source_map(&mut rendered, verified.module());
        print!("{rendered}");
    }
    print_success(
        format,
        "build",
        &json!({"source": source, "output": output}),
        &format!("built {} -> {}", source.display(), output.display()),
    );
    Ok(())
}

fn verify_command(arguments: &[String], format: DiagnosticFormat) -> Result<(), String> {
    let (path, limits) = parse_input_and_limits(arguments, "verify")?;
    verify_module_with_limits(&path, limits)?;
    print_success(
        format,
        "verify",
        &json!({"module": path}),
        &format!("verified {}", path.display()),
    );
    Ok(())
}

fn run_command(arguments: &[String], format: DiagnosticFormat, trace: bool) -> Result<(), String> {
    let mut input = None;
    let mut limits_file = None;
    let mut trace_output = None;
    let mut fuel = 1_000_000_u64;
    let mut function = 0_u32;
    let mut runtime_arguments = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        match option {
            "--limits-file" | "--trace-output" | "--fuel" | "--function" | "--arg-i32" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for `{option}`"))?;
                match option {
                    "--limits-file" => limits_file = Some(PathBuf::from(value)),
                    "--trace-output" => trace_output = Some(PathBuf::from(value)),
                    "--fuel" => fuel = parse_limit(option, value)?,
                    "--function" => function = parse_limit(option, value)?,
                    "--arg-i32" => {
                        runtime_arguments
                            .push(nexa_runtime::RuntimeValue::I32(parse_limit(option, value)?));
                    }
                    _ => unreachable!(),
                }
                index += 2;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown run option `{option}`"));
            }
            path if input.is_none() => {
                input = Some(PathBuf::from(path));
                index += 1;
            }
            path => return Err(format!("unexpected run argument `{path}`")),
        }
    }
    let input = input.ok_or("usage: nexa run|trace <source.nexa|module.nxb>")?;
    let limits = load_verifier_limits(limits_file.as_deref())?;
    let verified = load_verified(&input, limits)?;
    let outcome =
        nexa_runtime::CheckedInterpreter::run(&verified, function, &runtime_arguments, fuel)
            .map_err(|error| format!("execution failed: {error}"))?;
    let record = json!({
        "input": input,
        "function": function,
        "arguments": runtime_arguments.len(),
        "fuel_limit": fuel,
        "outcome": format!("{outcome:?}"),
    });
    if trace {
        let rendered = serde_json::to_string_pretty(&record)
            .map_err(|error| format!("could not serialize trace: {error}"))?;
        if let Some(path) = trace_output {
            std::fs::write(&path, rendered)
                .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        } else {
            println!("{rendered}");
        }
    } else {
        print_success(
            format,
            "run",
            &record,
            &format!("run completed: {outcome:?}"),
        );
    }
    Ok(())
}

fn fixture_check(arguments: &[String], format: DiagnosticFormat) -> Result<(), String> {
    let [path] = arguments else {
        return Err("usage: nexa fixture-check <fixture.json|directory>".into());
    };
    let path = Path::new(path);
    let paths = if path.is_dir() {
        files_with_extension(path, "json")?
    } else {
        vec![path.to_path_buf()]
    };
    if paths.is_empty() {
        return Err(format!("no JSON fixtures found under {}", path.display()));
    }
    for fixture in &paths {
        let bytes = std::fs::read(fixture)
            .map_err(|error| format!("could not read {}: {error}", fixture.display()))?;
        nexa_migrate::parse_state_fixture(&bytes, nexa_migrate::StateFixtureLimits::default())
            .map_err(|error| format!("{}: {error}", fixture.display()))?;
    }
    print_success(
        format,
        "fixture-check",
        &json!({"path": path, "fixtures": paths.len()}),
        &format!("validated {} migration fixtures", paths.len()),
    );
    Ok(())
}

fn model_replay(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let artifact: ModelFailureArtifact = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid model artifact: {error}"))?;
    if artifact.format_version != MODEL_FAILURE_ARTIFACT_VERSION {
        return Err(format!(
            "unsupported model artifact version {}",
            artifact.format_version
        ));
    }
    if artifact.path.last() != Some(&artifact.failure_event) {
        return Err("model artifact failure_event is not the final path event".into());
    }
    let model = artifact
        .model_config
        .get("model")
        .and_then(Value::as_str)
        .ok_or("model artifact has no model name")?;
    let visited = match model {
        "realm" => 4,
        name => {
            let (_, spec) = load_specs()?
                .into_iter()
                .find(|(_, spec)| spec.name == name)
                .ok_or_else(|| format!("unknown replay model `{name}`"))?;
            explore(&spec).visited_snapshots
        }
    };
    println!(
        "replayed {} against {model}: {} path events, {visited} explored states",
        path.display(),
        artifact.path.len()
    );
    Ok(())
}

fn parse_input_and_limits(
    arguments: &[String],
    command: &str,
) -> Result<(PathBuf, nexa_verifier::VerifierLimits), String> {
    let mut path = None;
    let mut limits_file = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--limits-file" => {
                limits_file = Some(PathBuf::from(
                    arguments
                        .get(index + 1)
                        .ok_or("missing value for `--limits-file`")?,
                ));
                index += 2;
            }
            option if option.starts_with('-') => {
                return Err(format!("unknown {command} option `{option}`"));
            }
            value if path.is_none() => {
                path = Some(PathBuf::from(value));
                index += 1;
            }
            value => return Err(format!("unexpected {command} argument `{value}`")),
        }
    }
    Ok((
        path.ok_or_else(|| format!("usage: nexa {command} <path>"))?,
        load_verifier_limits(limits_file.as_deref())?,
    ))
}

fn load_verifier_limits(path: Option<&Path>) -> Result<nexa_verifier::VerifierLimits, String> {
    let Some(path) = path else {
        return Ok(nexa_verifier::VerifierLimits::default());
    };
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid limits file {}: {error}", path.display()))?;
    let number = |name: &str, fallback: u32| -> Result<u32, String> {
        value.get(name).map_or(Ok(fallback), |value| {
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| format!("limits field `{name}` must be a u32"))
        })
    };
    let defaults = nexa_verifier::VerifierLimits::default();
    Ok(nexa_verifier::VerifierLimits {
        max_frame_bytes: number("max_frame_bytes", defaults.max_frame_bytes)?,
        max_immediate_cost: number("max_immediate_cost", defaults.max_immediate_cost)?,
        max_wcet_states: number("max_wcet_states", defaults.max_wcet_states)?,
    })
}

fn compile_source(path: &Path) -> Result<nexa_verifier::VerifiedModule, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    nexa_compiler::compile(&source).map_err(|error| error.to_string())
}

fn load_verified(
    path: &Path,
    limits: nexa_verifier::VerifierLimits,
) -> Result<nexa_verifier::VerifiedModule, String> {
    if path.extension().is_some_and(|extension| extension == "nxb") {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let module = nexa_bytecode::Module::decode(&bytes)
            .map_err(|error| format!("bytecode decode failed: {error}"))?;
        verify_with_limits(module, limits)
    } else {
        let verified = compile_source(path)?;
        verify_with_limits(verified.module().clone(), limits)
    }
}

fn verify_with_limits(
    module: nexa_bytecode::Module,
    limits: nexa_verifier::VerifierLimits,
) -> Result<nexa_verifier::VerifiedModule, String> {
    nexa_verifier::verify(module, limits)
        .map_err(|error| format!("bytecode verification failed: {error}"))
}

fn verify_module_with_limits(
    path: &Path,
    limits: nexa_verifier::VerifierLimits,
) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let module = nexa_bytecode::Module::decode(&bytes)
        .map_err(|error| format!("bytecode decode failed: {error}"))?;
    verify_with_limits(module, limits)?;
    Ok(())
}

fn print_success(format: DiagnosticFormat, command: &str, data: &Value, human: &str) {
    match format {
        DiagnosticFormat::Human => println!("{human}"),
        DiagnosticFormat::Json => println!(
            "{}",
            serde_json::to_string(&json!({
                "status": "ok",
                "command": command,
                "data": data,
            }))
            .expect("diagnostic JSON serialization does not fail")
        ),
    }
}

fn dump_module(arguments: &[String]) -> Result<(), String> {
    let (section, source_map_only, path) = match arguments {
        [path] => (None, false, Path::new(path)),
        [option, section, path] if option == "--section" => (
            Some(
                nexa_bytecode::SectionKind::from_name(section)
                    .ok_or_else(|| format!("unknown bytecode section `{section}`"))?,
            ),
            false,
            Path::new(path),
        ),
        [option, path] if option == "--source-map" || option == "--dump-source-map" => {
            (None, true, Path::new(path))
        }
        _ => {
            return Err(
                "usage: nexa dump [--section NAME|--dump-source-map] <module.nxb>".to_owned(),
            );
        }
    };
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let module = nexa_bytecode::Module::decode(&bytes)
        .map_err(|error| format!("bytecode decode failed: {error}"))?;
    let rendered = render_module_dump(&bytes, &module, section, source_map_only)?;
    print!("{rendered}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn render_module_dump(
    bytes: &[u8],
    module: &nexa_bytecode::Module,
    selected: Option<nexa_bytecode::SectionKind>,
    source_map_only: bool,
) -> Result<String, String> {
    use std::fmt::Write as _;

    let directory = nexa_bytecode::Module::inspect_section_directory(
        bytes,
        nexa_bytecode::DecodeLimits::default(),
    )
    .map_err(|error| format!("bytecode directory inspection failed: {error}"))?;
    let mut output = String::new();
    if source_map_only {
        render_source_map(&mut output, module);
        return Ok(output);
    }
    writeln!(
        output,
        "header magic=NXBC version={} sections={}",
        nexa_bytecode::BYTECODE_VERSION,
        directory.len()
    )
    .expect("String writes do not fail");
    for entry in &directory {
        let name = nexa_bytecode::SectionKind::ALL
            .into_iter()
            .find(|kind| *kind as u16 == entry.kind)
            .map_or("unknown", nexa_bytecode::SectionKind::name);
        if selected.is_none_or(|kind| kind as u16 == entry.kind) {
            writeln!(
                output,
                "section {name} kind={} flags={} offset={} length={} count={} checksum={:08x}",
                entry.kind, entry.flags, entry.offset, entry.length, entry.count, entry.checksum
            )
            .expect("String writes do not fail");
        }
    }
    let render = |kind| selected.is_none_or(|selected| selected == kind);
    if render(nexa_bytecode::SectionKind::Strings) {
        for (index, string) in module.strings.iter().enumerate() {
            writeln!(output, "string {index} {string:?}").expect("String writes do not fail");
        }
    }
    if render(nexa_bytecode::SectionKind::Types) {
        let mut handles = module.state_handle_types.iter().collect::<Vec<_>>();
        handles.sort_by_key(|handle| handle.type_id);
        for handle in handles {
            writeln!(
                output,
                "state-handle {:016x} target={:?}",
                handle.type_id.0, handle.target
            )
            .expect("String writes do not fail");
        }
        let mut arrays = module.array_types.iter().collect::<Vec<_>>();
        arrays.sort_by_key(|array| array.type_id);
        for array in arrays {
            writeln!(
                output,
                "array {:016x} element={:?}",
                array.type_id.0, array.element
            )
            .expect("String writes do not fail");
        }
        let mut maps = module.map_types.iter().collect::<Vec<_>>();
        maps.sort_by_key(|map| map.type_id);
        for map in maps {
            writeln!(
                output,
                "map {:016x} key={:?} value={:?}",
                map.type_id.0, map.key, map.value
            )
            .expect("String writes do not fail");
        }
        let mut buffers = module.buffer_types.iter().collect::<Vec<_>>();
        buffers.sort_by_key(|buffer| buffer.type_id);
        for buffer in buffers {
            writeln!(
                output,
                "buffer {:016x} element={:?} ownership=vm-copy",
                buffer.type_id.0, buffer.element
            )
            .expect("String writes do not fail");
        }
        let mut snapshots = module.snapshot_types.iter().collect::<Vec<_>>();
        snapshots.sort_by_key(|snapshot| snapshot.type_id);
        for snapshot in snapshots {
            writeln!(
                output,
                "snapshot {:016x} content-type={:016x} ownership=host immutable=true",
                snapshot.type_id.0, snapshot.content_type.0
            )
            .expect("String writes do not fail");
        }
    }
    if render(nexa_bytecode::SectionKind::Functions) {
        for (index, function) in module.functions.iter().enumerate() {
            writeln!(
                output,
                "function {index} effect={:?} registers={} frame_bytes={} signature={:?}",
                function.effect, function.registers, function.frame_bytes, function.signature
            )
            .expect("String writes do not fail");
        }
    }
    if render(nexa_bytecode::SectionKind::Code) {
        for (function, body) in module.functions.iter().enumerate() {
            writeln!(output, "code function={function}").expect("String writes do not fail");
            for (pc, instruction) in body.code.iter().enumerate() {
                writeln!(output, "  {pc:06} {instruction:?}").expect("String writes do not fail");
            }
        }
    }
    if render(nexa_bytecode::SectionKind::Enums) {
        let mut enums = module.enum_types.iter().collect::<Vec<_>>();
        enums.sort_by_key(|enum_type| enum_type.type_id);
        for enum_type in enums {
            writeln!(output, "enum {:016x}", enum_type.type_id.0)
                .expect("String writes do not fail");
            let mut variants = enum_type.variants.iter().collect::<Vec<_>>();
            variants.sort_by_key(|variant| (variant.tag, variant.stable_id));
            for variant in variants {
                writeln!(
                    output,
                    "  variant tag={} id={:016x} payload={:?}",
                    variant.tag, variant.stable_id.0, variant.payload_type
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa_bytecode::SectionKind::Structs) {
        let mut structs = module.struct_types.iter().collect::<Vec<_>>();
        structs.sort_by_key(|struct_type| struct_type.type_id);
        for struct_type in structs {
            writeln!(output, "struct {:016x}", struct_type.type_id.0)
                .expect("String writes do not fail");
            for (index, field) in struct_type.fields.iter().enumerate() {
                writeln!(
                    output,
                    "  field index={index} id={:016x} type={:?}",
                    field.stable_id.0, field.ty,
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa_bytecode::SectionKind::Classes) {
        let mut classes = module.class_types.iter().collect::<Vec<_>>();
        classes.sort_by_key(|class_type| class_type.type_id);
        for class_type in classes {
            writeln!(output, "class {:016x}", class_type.type_id.0)
                .expect("String writes do not fail");
            for (index, field) in class_type.fields.iter().enumerate() {
                writeln!(
                    output,
                    "  field index={index} id={:016x} type={:?} mutable=true",
                    field.stable_id.0, field.ty,
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa_bytecode::SectionKind::StateSchemas) {
        let mut state_types = module.state_schema.types.iter().collect::<Vec<_>>();
        state_types.sort_by_key(|state_type| state_type.stable_id);
        for state_type in state_types {
            writeln!(
                output,
                "stateful-class {:016x} version={}",
                state_type.stable_id.0, state_type.version
            )
            .expect("String writes do not fail");
            let mut fields = state_type.fields.iter().collect::<Vec<_>>();
            fields.sort_by_key(|field| field.stable_id);
            for field in fields {
                writeln!(
                    output,
                    "  field id={:016x} type={:?} persistent=true",
                    field.stable_id.0, field.ty
                )
                .expect("String writes do not fail");
            }
        }
    }
    if render(nexa_bytecode::SectionKind::SourceMap) {
        render_source_map(&mut output, module);
    }
    Ok(output)
}

fn render_source_map(output: &mut String, module: &nexa_bytecode::Module) {
    use std::fmt::Write as _;

    let mut entries = module.source_map.clone();
    entries.sort_by_key(|entry| {
        (
            entry.function,
            entry.pc_start,
            entry.pc_end,
            entry.span.file,
            entry.span.start,
            entry.span.end,
        )
    });
    for entry in entries {
        writeln!(
            output,
            "source-map function={} pc={}..{} file={} span={}..{}",
            entry.function,
            entry.pc_start,
            entry.pc_end,
            entry.span.file.0,
            entry.span.start,
            entry.span.end
        )
        .expect("String writes do not fail");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrateOutputFormat {
    Human,
    Json,
}

struct MigrateCommand {
    old_module: PathBuf,
    new_module: PathBuf,
    state: PathBuf,
    output: Option<PathBuf>,
    format: MigrateOutputFormat,
    config: nexa_migrate::MigrateCheckConfig,
}

fn parse_migrate_command(arguments: &[String]) -> Result<MigrateCommand, String> {
    let mut old_module = None;
    let mut new_module = None;
    let mut state = None;
    let mut output = None;
    let mut format = MigrateOutputFormat::Human;
    let mut config = nexa_migrate::MigrateCheckConfig::default();
    let mut index = 0;
    while index < arguments.len() {
        let option = arguments[index].as_str();
        match option {
            "--dump-state" => {
                config.dump_state = true;
                index += 1;
                continue;
            }
            "--diff-state" => {
                config.diff_state = true;
                index += 1;
                continue;
            }
            _ => {}
        }
        let value = arguments
            .get(index + 1)
            .ok_or_else(|| format!("missing value for `{option}`"))?;
        match option {
            "--old-module" => old_module = Some(PathBuf::from(value)),
            "--new-module" => new_module = Some(PathBuf::from(value)),
            "--state" => state = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            "--format" => {
                format = match value.as_str() {
                    "human" => MigrateOutputFormat::Human,
                    "json" => MigrateOutputFormat::Json,
                    _ => return Err("`--format` must be `human` or `json`".into()),
                };
            }
            "--max-objects" => {
                config.migration_limits.max_objects = parse_limit(option, value)?;
            }
            "--max-fields" => {
                config.migration_limits.max_fields = parse_limit(option, value)?;
            }
            "--max-forwarding-entries" => {
                config.migration_limits.max_forwarding_entries = parse_limit(option, value)?;
            }
            "--max-state-bytes" => {
                config.migration_limits.max_state_bytes = parse_limit(option, value)?;
            }
            "--max-gc-roots" => {
                config.migration_limits.max_gc_roots = parse_limit(option, value)?;
            }
            "--max-fuel" => {
                config.migration_limits.max_fuel = parse_limit(option, value)?;
            }
            "--max-call-depth" => {
                config.migration_limits.max_call_depth = parse_limit(option, value)?;
            }
            _ => return Err(format!("unknown migrate-check option `{option}`")),
        }
        index += 2;
    }
    Ok(MigrateCommand {
        old_module: old_module.ok_or("missing `--old-module`")?,
        new_module: new_module.ok_or("missing `--new-module`")?,
        state: state.ok_or("missing `--state`")?,
        output,
        format,
        config,
    })
}

fn render_migrate_result(
    result: &nexa_migrate::MigrateCheckResult,
    format: MigrateOutputFormat,
) -> Result<String, String> {
    match format {
        MigrateOutputFormat::Human => {
            let mut rendered = format!(
                "migration check passed\n\
             old schema: {:016x}\n\
             new schema: {:016x}\n\
             migration entry: {}\n\
             migration hash: {:016x}\n\
             final state hash: {:016x}\n\
             objects: {}\n\
             objects read/created: {}/{}\n\
             fields written: {}\n\
             preserve/replace/delete: {}/{}/{}\n\
             generation changes: {}\n\
             handle remaps: {}\n\
             peak objects/fields/forwarding: {}/{}/{}\n\
             peak state bytes/GC roots: {}/{}\n\
             fuel: {}\n\
             call depth: {}\n",
                result.old_schema_hash,
                result.new_schema_hash,
                result.migration_entry,
                result.migration_hash,
                result.final_state_hash,
                result.final_object_count,
                result.usage.objects_read,
                result.usage.objects_created,
                result.usage.fields_written,
                result.usage.preserved,
                result.usage.replaced,
                result.usage.deleted,
                result.usage.generation_changes,
                result.usage.handle_remaps,
                result.usage.object_peak,
                result.usage.field_peak,
                result.usage.forwarding_peak,
                result.usage.payload_byte_peak,
                result.usage.gc_root_peak,
                result.usage.fuel_used,
                result.usage.max_call_depth_used,
            );
            if let Some(diff) = &result.state_diff {
                use std::fmt::Write as _;
                writeln!(
                    rendered,
                    "diff added/removed/changed: {}/{}/{}",
                    diff.added_objects.len(),
                    diff.removed_objects.len(),
                    diff.changed_objects.len()
                )
                .expect("String writes do not fail");
            }
            if let Some(state) = &result.output_state {
                rendered.push_str("state:\n");
                rendered.push_str(
                    &serde_json::to_string_pretty(state)
                        .map_err(|error| format!("could not serialize output state: {error}"))?,
                );
                rendered.push('\n');
            }
            Ok(rendered)
        }
        MigrateOutputFormat::Json => serde_json::to_string_pretty(result)
            .map_err(|error| format!("could not serialize migration result: {error}")),
    }
}

fn migrate_check(arguments: &[String]) -> Result<(), String> {
    let command = parse_migrate_command(arguments)?;
    let old_bytes = std::fs::read(&command.old_module)
        .map_err(|error| format!("could not read {}: {error}", command.old_module.display()))?;
    let new_bytes = std::fs::read(&command.new_module)
        .map_err(|error| format!("could not read {}: {error}", command.new_module.display()))?;
    let state_bytes = std::fs::read(&command.state)
        .map_err(|error| format!("could not read {}: {error}", command.state.display()))?;
    let result =
        nexa_migrate::run_migrate_check(&old_bytes, &new_bytes, &state_bytes, command.config)
            .map_err(|error| format!("migration check failed: {error}"))?;
    let rendered = render_migrate_result(&result, command.format)?;
    if let Some(output) = command.output {
        std::fs::write(&output, rendered)
            .map_err(|error| format!("could not write {}: {error}", output.display()))?;
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn parse_limit<T>(option: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| format!("invalid numeric value for `{option}`: `{value}`"))
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

#[allow(clippy::too_many_lines)]
fn check_models() -> Result<(), String> {
    let artifact = Path::new("target/model-artifacts/shortest-failure.json");
    if let Some(parent) = artifact.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create model artifact directory: {error}"))?;
    }
    let specs = load_specs()?;
    let mut snapshot_count = 0;
    for (_, spec) in &specs {
        let report = explore(spec);
        if !report.is_success(spec) {
            let (message, path) = report.failures.first().map_or_else(
                || {
                    (
                        "coverage failure without transition path".to_owned(),
                        Vec::new(),
                    )
                },
                |failure| (failure.message.clone(), failure.path.clone()),
            );
            write_exploration_failure(
                artifact,
                &spec.name,
                &json!({"max_depth": 256, "max_snapshots": 100_000}),
                &path,
                &message,
                "NEXA_MODEL_MACHINE_FAILURE",
            )?;
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
        write_exploration_failure(
            artifact,
            "task-scope-system",
            &json!({"max_depth": 32, "max_worlds": 16_384}),
            &path
                .iter()
                .map(|event| format!("{event:?}"))
                .collect::<Vec<_>>(),
            message,
            "NEXA_MODEL_SYSTEM_FAILURE",
        )?;
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
        let (message, path) = &realm_report.failures[0];
        write_exploration_failure(
            artifact,
            "realm-runtime-system",
            &json!({}),
            &path
                .iter()
                .map(|event| format!("{event:?}"))
                .collect::<Vec<_>>(),
            message,
            "NEXA_MODEL_REALM_FAILURE",
        )?;
        return Err(format!(
            "RealmRuntime system model failed: {:?}",
            realm_report.failures
        ));
    }
    let mut current_realm = RealmModel::default();
    for event in [
        RealmEvent::Spawn,
        RealmEvent::Poll,
        RealmEvent::RestartReload,
        RealmEvent::LateCompletion,
    ] {
        current_realm
            .apply(event)
            .map_err(|error| format!("current Realm model rejected {event:?}: {error:?}"))?;
    }
    if !current_realm.invariants_hold() {
        return Err("current Realm model violated resource invariants".into());
    }
    let summary = std::fs::File::create("target/model-artifacts/model-check-summary.json")
        .map_err(|error| format!("could not create model summary: {error}"))?;
    serde_json::to_writer_pretty(
        summary,
        &json!({
            "format_version": 1,
            "commit_sha": current_commit_sha(),
            "status": "success",
            "machine_snapshots": snapshot_count,
            "task_scope_worlds": system_report.visited_worlds,
            "realm_worlds": realm_report.visited_worlds,
            "current_realm_paths": 1
        }),
    )
    .map_err(|error| format!("could not write model summary: {error}"))?;
    println!(
        "bounded model exploration passed: {} machines, {snapshot_count} snapshots, {} task/scope worlds, {} realm worlds, current Realm restart path passed",
        specs.len(),
        system_report.visited_worlds,
        realm_report.visited_worlds,
    );
    Ok(())
}

fn write_exploration_failure(
    path: &Path,
    model: &str,
    model_config: &Value,
    trace: &[String],
    message: &str,
    error_code: &str,
) -> Result<(), String> {
    let failure_event = trace
        .last()
        .cloned()
        .unwrap_or_else(|| "exploration".into());
    let artifact = ModelFailureArtifact {
        format_version: MODEL_FAILURE_ARTIFACT_VERSION,
        commit_sha: current_commit_sha(),
        runtime_kind: "RealmRuntime".into(),
        shadow_state_fields: 0,
        model_config: json!({
            "model": model,
            "bounds": model_config
        }),
        path: trace.to_owned(),
        failure_event,
        model_before: Value::Null,
        model_after: json!({"failure": message}),
        runtime_before: Value::Null,
        runtime_after: Value::Null,
        ledger: json!({}),
        epochs: json!({}),
        tasks: json!([]),
        requests: json!([]),
        completions: json!([]),
        releases: json!([]),
        heap: json!({}),
        roots: json!([]),
        root_publications: json!([]),
        module_handles: json!([]),
        completion_accounting: json!({}),
        failure_point_stats: json!({}),
        trace: json!(trace),
        error_code: error_code.into(),
    };
    let file = std::fs::File::create(path)
        .map_err(|error| format!("could not create model failure artifact: {error}"))?;
    write_model_failure_artifact(file, &artifact)
        .map_err(|error| format!("could not write model failure artifact: {error}"))
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use nexa_bytecode::{
        ArrayType, BufferType, ClassType, FunctionBuilder, Instruction, MapType, ModuleBuilder,
        SectionKind, Signature, SnapshotType, SourceMapEntry, StateField, StateHandleType,
        StateSchema, StateType, StructField, StructType, ValueType,
    };
    use nexa_core::{FileId, SourceSpan, StableId};

    use nexa_model::artifact::{
        MODEL_FAILURE_ARTIFACT_VERSION, ModelFailureArtifact, write_model_failure_artifact,
    };
    use serde_json::json;

    use super::{
        DiagnosticFormat, build_command, check_command, extract_diagnostic_format, fixture_check,
        model_replay, render_module_dump, run_command, verify_command,
    };

    #[test]
    fn complete_cli_command_paths_execute_real_components() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("nexa-cli-{nonce}-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("main.nexa");
        let module = directory.join("main.nxb");
        let trace = directory.join("trace.json");
        let limits = directory.join("limits.json");
        let fixture = directory.join("fixture.json");
        fs::write(&source, "fn main() -> i32 { return 7; }").unwrap();
        fs::write(
            &limits,
            r#"{"max_frame_bytes":65536,"max_immediate_cost":1024,"max_wcet_states":100000}"#,
        )
        .unwrap();
        fs::write(
            &fixture,
            r#"{"format_version":1,"stateful_domain":7,"objects":[]}"#,
        )
        .unwrap();

        check_command(
            &[
                source.display().to_string(),
                "--limits-file".into(),
                limits.display().to_string(),
            ],
            DiagnosticFormat::Json,
        )
        .unwrap();
        build_command(
            &[
                source.display().to_string(),
                "-o".into(),
                module.display().to_string(),
                "--dump-source-map".into(),
            ],
            DiagnosticFormat::Human,
        )
        .unwrap();
        verify_command(
            &[
                module.display().to_string(),
                "--limits-file".into(),
                limits.display().to_string(),
            ],
            DiagnosticFormat::Human,
        )
        .unwrap();
        run_command(
            &[
                module.display().to_string(),
                "--trace-output".into(),
                trace.display().to_string(),
            ],
            DiagnosticFormat::Human,
            true,
        )
        .unwrap();
        assert!(fs::read_to_string(&trace).unwrap().contains("I32(7)"));
        fixture_check(&[fixture.display().to_string()], DiagnosticFormat::Human).unwrap();

        let artifact_path = directory.join("model.json");
        let artifact = ModelFailureArtifact {
            format_version: MODEL_FAILURE_ARTIFACT_VERSION,
            commit_sha: "test".into(),
            runtime_kind: "RealmRuntime".into(),
            shadow_state_fields: 0,
            model_config: json!({"model": "realm"}),
            path: vec!["Spawn".into()],
            failure_event: "Spawn".into(),
            model_before: json!({}),
            model_after: json!({}),
            runtime_before: json!({}),
            runtime_after: json!({}),
            ledger: json!({}),
            epochs: json!({}),
            tasks: json!([]),
            requests: json!([]),
            completions: json!([]),
            releases: json!([]),
            heap: json!({}),
            roots: json!([]),
            root_publications: json!([]),
            module_handles: json!([]),
            completion_accounting: json!({}),
            failure_point_stats: json!({}),
            trace: json!([]),
            error_code: "NEXA_MODEL_TEST".into(),
        };
        let file = fs::File::create(&artifact_path).unwrap();
        write_model_failure_artifact(file, &artifact).unwrap();
        model_replay(&artifact_path).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn diagnostic_format_is_global_and_strict() {
        let (format, arguments) = extract_diagnostic_format(&[
            "check".into(),
            "main.nexa".into(),
            "--diagnostic-format".into(),
            "json".into(),
        ])
        .unwrap();
        assert_eq!(format, DiagnosticFormat::Json);
        assert_eq!(arguments, ["check", "main.nexa"]);
        assert!(
            extract_diagnostic_format(&[
                "check".into(),
                "--diagnostic-format".into(),
                "xml".into(),
            ])
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn bytecode_dump_is_deterministic_and_supports_code_types_and_source_map_views() {
        let mut function = FunctionBuilder::new(
            Signature {
                parameters: Vec::new(),
                result: Some(ValueType::I32),
            },
            1,
        );
        function
            .emit(Instruction::LoadI32 { dst: 0, value: 7 })
            .emit(Instruction::Return { source: 0 });
        let mut builder = ModuleBuilder::new();
        builder.string("Nexa界\n");
        let position_type = StableId::from_name("Position");
        builder.struct_type(StructType {
            type_id: position_type,
            fields: vec![StructField {
                stable_id: StableId::from_parts(&["Position", "::x"]),
                ty: ValueType::I32,
            }],
        });
        builder.class_type(ClassType {
            type_id: StableId::from_name("Node"),
            fields: vec![StructField {
                stable_id: StableId::from_parts(&["Node", "::value"]),
                ty: ValueType::I32,
            }],
        });
        let state_target = ValueType::Named(StableId::from_name("Store"));
        builder.state_handle_type(StateHandleType::new(state_target));
        builder.array_type(ArrayType::new(ValueType::I32));
        builder.map_type(MapType::new(ValueType::String, ValueType::I32));
        builder.buffer_type(BufferType::new(ValueType::I64));
        builder.snapshot_type(SnapshotType::new(position_type));
        builder.state_schema(StateSchema {
            types: vec![StateType {
                stable_id: StableId::from_name("Store"),
                version: 1,
                fields: vec![StateField {
                    stable_id: StableId::from_parts(&["Store", "::value"]),
                    ty: ValueType::I32,
                }],
            }],
        });
        builder.function(function.finish().unwrap());
        builder.source_map([
            SourceMapEntry {
                function: 0,
                pc_start: 1,
                pc_end: 2,
                span: SourceSpan::new(FileId(1), 20, 26),
            },
            SourceMapEntry {
                function: 0,
                pc_start: 0,
                pc_end: 1,
                span: SourceSpan::new(FileId(1), 10, 19),
            },
        ]);
        let module = builder.finish();
        let bytes = module.encode();

        let full = render_module_dump(&bytes, &module, None, false).unwrap();
        assert_eq!(
            full,
            render_module_dump(&bytes, &module, None, false).unwrap()
        );
        assert!(full.contains("header magic=NXBC version=4 sections=16"));
        assert!(full.contains("000000 LoadI32"));
        assert!(full.contains("string 0 \"Nexa界\\n\""));
        assert!(full.contains("struct "));
        assert!(full.contains("field index=0"));
        assert!(full.contains("class "));
        assert!(full.contains("mutable=true"));
        assert!(full.contains("state-handle "));
        assert!(full.contains("array "));
        assert!(full.contains("element=I32"));
        assert!(full.contains("map "));
        assert!(full.contains("key=String value=I32"));
        assert!(full.contains("buffer "));
        assert!(full.contains("element=I64 ownership=vm-copy"));
        assert!(full.contains("snapshot "));
        assert!(full.contains("ownership=host immutable=true"));
        assert!(full.contains("stateful-class "));
        assert!(full.contains("persistent=true"));
        assert!(
            full.find("pc=0..1")
                .expect("first source map entry is present")
                < full
                    .find("pc=1..2")
                    .expect("second source map entry is present")
        );

        let types = render_module_dump(&bytes, &module, Some(SectionKind::Types), false).unwrap();
        assert!(types.contains("section types"));
        assert!(types.contains("state-handle "));
        assert!(types.contains("array "));
        assert!(types.contains("map "));
        assert!(types.contains("buffer "));
        assert!(types.contains("snapshot "));
        assert!(!types.contains("code function="));
        let code = render_module_dump(&bytes, &module, Some(SectionKind::Code), false).unwrap();
        assert!(code.contains("section code"));
        assert!(code.contains("000001 Return"));
        let source_map = render_module_dump(&bytes, &module, None, true).unwrap();
        assert!(source_map.starts_with("source-map function=0 pc=0..1"));
    }
}
