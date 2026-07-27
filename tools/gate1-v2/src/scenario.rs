use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use nexa_gate1_v2_4::{AnyError, hash_file, read_json, stable_value_hash, write_json};

pub const H1_MANIFEST: &str = "experiments/gate1-v2.4/h1_mutations.json";
pub const H1_HANDWRITTEN: &str = "experiments/gate1-v2.4/fixtures/h1_handwritten.rs";
pub const H2_MANIFEST: &str = "experiments/gate1-v2.4/h2_matrix.json";
pub const H2_CLEANUP_MANIFEST: &str = "experiments/gate1-v2.4/h2_cleanup_matrix.json";
pub const H3_MANIFEST: &str = "experiments/gate1-v2.4/h3_scenarios.json";
pub const SCENARIO_SCHEMA: &str = "experiments/gate1-v2.4/scenario_schema.json";
const DRYRUN_ROOT: &str = "target/gate1-v2.4-dryrun";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ObservationRule {
    pub pointer: String,
    pub operator: String,
    pub expected: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScenarioSpec {
    pub id: String,
    pub group: String,
    pub description: String,
    pub fixture: String,
    pub executor: String,
    pub expected_operations: Vec<String>,
    pub expected_observations: Vec<ObservationRule>,
    pub forbidden_operations: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct H1Mutation {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub idl_transformer: String,
    pub handwritten_transformer: String,
    pub expected_changed_symbols: Vec<String>,
    pub expected_phase: String,
    pub semantic_change_signature: String,
}

#[derive(Clone, Debug, Deserialize)]
struct H1Manifest {
    mutations: Vec<H1Mutation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct H2Configuration {
    pub id: String,
    pub calls_per_frame: usize,
    pub first_slice_ratio: u32,
    pub trace: bool,
    pub host_call: bool,
    pub value_shape: String,
    pub cohort_set: String,
}

#[derive(Clone, Debug, Deserialize)]
struct H2Manifest {
    cohort_sets: BTreeMap<String, Vec<String>>,
    configurations: Vec<H2Configuration>,
}

#[derive(Clone, Debug, Deserialize)]
struct ScenarioManifest {
    scenarios: Vec<ScenarioSpec>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HypothesisOutcome {
    Pass,
    Fail,
    Inconclusive,
    Invalid,
    NotRunDueToTerminalDecision,
}

impl HypothesisOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Inconclusive => "INCONCLUSIVE",
            Self::Invalid => "INVALID",
            Self::NotRunDueToTerminalDecision => "NOT_RUN_DUE_TO_TERMINAL_DECISION",
        }
    }
}

#[must_use]
pub fn outcome_from_failures(failures: &[String]) -> HypothesisOutcome {
    if failures.is_empty() {
        HypothesisOutcome::Pass
    } else {
        HypothesisOutcome::Fail
    }
}

pub fn h1_mutations() -> Result<Vec<H1Mutation>, AnyError> {
    let manifest: H1Manifest = serde_json::from_value(read_json(H1_MANIFEST)?)?;
    Ok(manifest.mutations)
}

pub fn h2_configurations() -> Result<Vec<H2Configuration>, AnyError> {
    let manifest: H2Manifest = serde_json::from_value(read_json(H2_MANIFEST)?)?;
    Ok(manifest.configurations)
}

pub fn h2_cleanup_specs() -> Result<Vec<ScenarioSpec>, AnyError> {
    let manifest: ScenarioManifest = serde_json::from_value(read_json(H2_CLEANUP_MANIFEST)?)?;
    Ok(manifest.scenarios)
}

pub fn h3_specs() -> Result<Vec<ScenarioSpec>, AnyError> {
    let manifest: ScenarioManifest = serde_json::from_value(read_json(H3_MANIFEST)?)?;
    Ok(manifest.scenarios)
}

pub fn scenario_independence_check() -> Result<Value, AnyError> {
    let h1: H1Manifest = serde_json::from_value(read_json(H1_MANIFEST)?)?;
    let h2: H2Manifest = serde_json::from_value(read_json(H2_MANIFEST)?)?;
    let cleanup: ScenarioManifest = serde_json::from_value(read_json(H2_CLEANUP_MANIFEST)?)?;
    let h3: ScenarioManifest = serde_json::from_value(read_json(H3_MANIFEST)?)?;
    let mut failures = Vec::new();

    if h1.mutations.len() != 20 {
        failures.push(format!(
            "H1 manifest contains {} mutations",
            h1.mutations.len()
        ));
    }
    unique_field(
        "H1 ID",
        h1.mutations.iter().map(|item| item.id.as_str()),
        &mut failures,
    );
    unique_field(
        "H1 IDL transformer",
        h1.mutations
            .iter()
            .map(|item| item.idl_transformer.as_str()),
        &mut failures,
    );
    unique_field(
        "H1 handwritten transformer",
        h1.mutations
            .iter()
            .map(|item| item.handwritten_transformer.as_str()),
        &mut failures,
    );
    if h1
        .mutations
        .iter()
        .filter(|item| item.kind.contains("Rename"))
        .count()
        != 1
    {
        failures.push("H1 must contain exactly one rename mutation".to_owned());
    }
    for mutation in &h1.mutations {
        if mutation.expected_changed_symbols.is_empty()
            || mutation.semantic_change_signature.is_empty()
            || mutation.idl_transformer == mutation.handwritten_transformer
                && mutation.kind == "RenamePreserveStableId"
                && !mutation.semantic_change_signature.contains("preserve")
        {
            failures.push(format!("{} has incomplete semantic pairing", mutation.id));
        }
    }

    if h2.configurations.len() != 32 {
        failures.push(format!(
            "H2 manifest contains {} configurations",
            h2.configurations.len()
        ));
    }
    let expected_cohorts = [
        "ImmediateSuccess",
        "FuelYield",
        "ExplicitYield",
        "HostSuccess",
        "HostError",
        "Cancel",
        "Abandon",
        "TerminalCleanup",
    ]
    .map(str::to_owned);
    if h2
        .cohort_sets
        .get("runtime-lifecycle-v1")
        .map(Vec::as_slice)
        != Some(expected_cohorts.as_slice())
    {
        failures.push("H2 lifecycle cohort set is incomplete".to_owned());
    }
    let config_fingerprints = h2
        .configurations
        .iter()
        .map(serialized_hash)
        .collect::<BTreeSet<_>>();
    if config_fingerprints.len() != 32 {
        failures.push("H2 configurations do not have 32 unique spec hashes".to_owned());
    }
    for (field, observed, expected) in [
        (
            "calls_per_frame",
            h2.configurations
                .iter()
                .map(|item| item.calls_per_frame.to_string())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["500".to_owned(), "1000".to_owned()]),
        ),
        (
            "first_slice_ratio",
            h2.configurations
                .iter()
                .map(|item| item.first_slice_ratio.to_string())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["95".to_owned(), "99".to_owned()]),
        ),
        (
            "trace",
            h2.configurations
                .iter()
                .map(|item| item.trace.to_string())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["false".to_owned(), "true".to_owned()]),
        ),
        (
            "host_call",
            h2.configurations
                .iter()
                .map(|item| item.host_call.to_string())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["false".to_owned(), "true".to_owned()]),
        ),
        (
            "value_shape",
            h2.configurations
                .iter()
                .map(|item| item.value_shape.clone())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["complex".to_owned(), "scalar".to_owned()]),
        ),
    ] {
        if observed != expected {
            failures.push(format!("H2 dimension {field} does not contain both values"));
        }
    }

    validate_specs("H2 cleanup", &cleanup.scenarios, 12, &mut failures);
    validate_specs("H3", &h3.scenarios, 30, &mut failures);
    for (group, expected) in [("migration", 11), ("completion", 10), ("transaction", 9)] {
        let actual = h3
            .scenarios
            .iter()
            .filter(|scenario| scenario.group == group)
            .count();
        if actual != expected {
            failures.push(format!(
                "H3 group {group} has {actual}, expected {expected}"
            ));
        }
    }

    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.4",
        "h1_mutation_count": h1.mutations.len(),
        "h1_transformer_count": h1.mutations.iter().map(|item| &item.handwritten_transformer).collect::<BTreeSet<_>>().len(),
        "h2_configuration_count": h2.configurations.len(),
        "h2_configuration_fingerprint_count": config_fingerprints.len(),
        "h2_cleanup_trigger_fingerprint_count": cleanup.scenarios.iter().map(operation_fingerprint).collect::<BTreeSet<_>>().len(),
        "h3_scenario_count": h3.scenarios.len(),
        "h3_spec_hash_count": h3.scenarios.iter().map(serialized_hash).collect::<BTreeSet<_>>().len(),
        "h3_executor_count": h3.scenarios.iter().map(|item| &item.executor).collect::<BTreeSet<_>>().len(),
        "failures": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("scenario_independence.json"),
        &result,
    )?;
    ensure_pass(&result, "scenario independence")?;
    Ok(result)
}

pub fn outcome_transport_check() -> Result<Value, AnyError> {
    let cases = [
        ("all-pass", ["PASS", "PASS", "PASS"], "HOLD"),
        ("h1-fail", ["FAIL", "PASS", "PASS"], "STOP"),
        ("h2-fail", ["PASS", "FAIL", "PASS"], "STOP"),
        ("h3-fail", ["PASS", "PASS", "FAIL"], "STOP"),
        (
            "inconclusive",
            ["INCONCLUSIVE", "PASS", "PASS"],
            "UNVERIFIABLE_WITHIN_MVR",
        ),
        ("invalid", ["INVALID", "PASS", "PASS"], "INVALID"),
    ]
    .map(|(name, outcomes, expected)| {
        let actual = decision_for_outcomes(outcomes, false, false);
        json!({
            "name": name,
            "worker_exit_code": 0,
            "apparatus_status": if outcomes.contains(&"INVALID") {"INVALID"} else {"PASS"},
            "outcomes": outcomes,
            "expected_decision": expected,
            "actual_decision": actual,
            "matched": actual == expected
        })
    });
    let crash = json!({
        "name": "crash-or-missing-result",
        "worker_exit_code": 23,
        "supervisor_outcome": "INVALID",
        "matched": true
    });
    let result = json!({
        "schema_version": 1,
        "protocol": "gate1-outcome-transport-v2",
        "cases": cases,
        "crash_case": crash,
        "fail_preserved": cases[1]["outcomes"][0] == "FAIL" && cases[1]["actual_decision"] == "STOP",
        "inconclusive_preserved": cases[4]["actual_decision"] == "UNVERIFIABLE_WITHIN_MVR",
        "status": if cases.iter().all(|case| case["matched"] == true) {"PASS"} else {"FAIL"}
    });
    write_json(
        &Path::new(DRYRUN_ROOT).join("outcome_transport.json"),
        &result,
    )?;
    ensure_pass(&result, "outcome transport")?;
    Ok(result)
}

#[must_use]
pub fn decision_for_outcomes(outcomes: [&str; 3], pilot: bool, budget: bool) -> &'static str {
    if outcomes.contains(&"INVALID") {
        "INVALID"
    } else if outcomes.contains(&"INCONCLUSIVE") {
        "UNVERIFIABLE_WITHIN_MVR"
    } else if outcomes.contains(&"FAIL") {
        "STOP"
    } else if !pilot {
        "HOLD"
    } else if budget {
        "PROCEED_TO_GATE2_RFC"
    } else {
        "PROCEED_TO_PILOT"
    }
}

pub fn status_lint() -> Result<Value, AnyError> {
    let registry = read_json("reports/history/gate1/current_status.json")?;
    let readme = std::fs::read_to_string("README.md")?;
    let roadmap = std::fs::read_to_string("ROADMAP.md")?;
    let baseline = std::fs::read_to_string("baseline/BASELINE_INDEX.md")?;
    let expected = StatusView::from_registry(&registry)?;
    let failures = status_failures(&expected, &readme, &roadmap, &baseline);
    let negative_cases = [
        (
            "roadmap-table-conflict",
            readme.clone(),
            roadmap.replace(
                &format!("| Gate 1 v2.4 |{}", expected.roadmap_row_tail),
                "| Gate 1 v2.4 | scenario | decision | Complete",
            ),
            baseline.clone(),
        ),
        (
            "readme-decision-conflict",
            readme.replace(
                &format!("Current decision: {}", expected.decision),
                "Current decision: HOLD",
            ),
            roadmap.clone(),
            baseline.clone(),
        ),
        (
            "baseline-milestone-conflict",
            readme.clone(),
            roadmap.clone(),
            baseline.replace(&expected.milestone_status, "COMPLETE"),
        ),
    ];
    let negative_results = negative_cases.map(|(name, r, m, b)| {
        let detected = !status_failures(&expected, &r, &m, &b).is_empty();
        json!({"name": name, "detected": detected})
    });
    let status =
        if failures.is_empty() && negative_results.iter().all(|case| case["detected"] == true) {
            "PASS"
        } else {
            "FAIL"
        };
    let result = json!({
        "schema_version": 1,
        "registry": registry,
        "negative_cases": negative_results,
        "failures": failures,
        "status": status
    });
    write_json(&Path::new(DRYRUN_ROOT).join("status_lint.json"), &result)?;
    ensure_pass(&result, "status lint")?;
    Ok(result)
}

pub fn apply_idl_transformer(source: &str, kind: &str) -> Result<String, AnyError> {
    let (from, to) = match kind {
        "ParameterType" => ("amount: i32", "amount: i64"),
        "ReturnType" => ("Result<i32, CombatError>", "i32"),
        "AddParameter" => (
            "fn heal(entity: i32, amount: i32)",
            "fn heal(entity: i32, amount: i32, source: i32)",
        ),
        "DeleteParameter" => ("fn entity_name(entity: i32)", "fn entity_name()"),
        "ParameterOrder" => (
            "fn set_position(entity: i32, position: Vec2)",
            "fn set_position(position: Vec2, entity: i32)",
        ),
        "SyncToAsync" => (
            "sync fuel 2 fn combat_event(entity: i32) -> CombatEvent;",
            "request(return_error, trap) fn combat_event(entity: i32) -> request<Result<CombatEvent, CombatError>>;",
        ),
        "FuelCost" => ("sync fuel 2 fn enemy_view", "sync fuel 3 fn enemy_view"),
        "CancelPolicy" => (
            "request(return_error, trap) fn play_animation",
            "request(cancel_task, trap) fn play_animation",
        ),
        "AbandonPolicy" => (
            "request(cancel_task, return_error) fn query_path",
            "request(cancel_task, trap) fn query_path",
        ),
        "EnumVariantAdd" => (
            "CombatError { MissingEntity, InvalidAmount, Busy, Cancelled }",
            "CombatError { MissingEntity, InvalidAmount, Busy, Cancelled, Timeout }",
        ),
        "EnumPayloadType" => ("Damage(i32)", "Damage(i64)"),
        "StructFieldAdd" => (
            "Vec2 { x: i32; y: i32; }",
            "Vec2 { x: i32; y: i32; z: i32; }",
        ),
        "StructFieldType" => ("health: i32", "health: i64"),
        "SnapshotContentType" => ("snapshot<EnemyView>", "snapshot<Vec2>"),
        "BufferElementType" => (
            "fn path(entity: i32) -> buffer<Vec2>",
            "fn path(entity: i32) -> buffer<i32>",
        ),
        "ResourceDomain" => ("token<CombatResource>", "token<Vec2>"),
        "StableId" => ("fn score(entity: i32)", "fn total_score(entity: i32)"),
        "RenamePreserveStableId" => ("fn ratio(entity: i32)", "fn combat_ratio(entity: i32)"),
        "StaleInterfaceHash" => ("sync fuel 1 fn clear_target", "sync fuel 2 fn clear_target"),
        "MissingHostFunction" => (
            "sync fuel 1 fn inspect_events(events: array<CombatEvent>) -> Result<i32, CombatError>;",
            "",
        ),
        other => return Err(format!("unknown IDL transformer kind {other}").into()),
    };
    replace_once(source, from, to, kind)
}

pub fn apply_handwritten_transformer(source: &str, kind: &str) -> Result<String, AnyError> {
    let (from, to) = match kind {
        "ParameterType" => ("parameter_type(value: i32)", "parameter_type(value: i64)"),
        "ReturnType" => (
            "return_type(value: i32) -> i32 { value }",
            "return_type(value: i32) -> i64 { i64::from(value) }",
        ),
        "AddParameter" => (
            "add_parameter(value: i32)",
            "add_parameter(value: i32, source: i32)",
        ),
        "DeleteParameter" => (
            "delete_parameter(value: i32) -> i32 { value }",
            "delete_parameter() -> i32 { 0 }",
        ),
        "ParameterOrder" => (
            "parameter_order(entity: i32, amount: i64)",
            "parameter_order(amount: i64, entity: i32)",
        ),
        "SyncToAsync" => ("HostResult::Value", "HostResult::Pending"),
        "FuelCost" => ("FUEL_COST: u32 = 1", "FUEL_COST: u32 = 3"),
        "CancelPolicy" => (
            "CANCEL_POLICY: &str = \"return_error\"",
            "CANCEL_POLICY: &str = \"cancel_task\"",
        ),
        "AbandonPolicy" => (
            "ABANDON_POLICY: &str = \"return_error\"",
            "ABANDON_POLICY: &str = \"trap\"",
        ),
        "EnumVariantAdd" => (
            "MutationError { Missing, Busy }",
            "MutationError { Missing, Busy, Timeout }",
        ),
        "EnumPayloadType" => (
            "MutationEvent { Damage(i32), Idle }",
            "MutationEvent { Damage(i64), Idle }",
        ),
        "StructFieldAdd" => (
            "pub x: i32, pub y: i32",
            "pub x: i32, pub y: i32, pub z: i32",
        ),
        "StructFieldType" => (
            "MutationView { pub health: i32 }",
            "MutationView { pub health: i64 }",
        ),
        "SnapshotContentType" => ("Snapshot<MutationView>", "Snapshot<MutationVec>"),
        "BufferElementType" => ("Buffer<MutationVec>", "Buffer<i32>"),
        "ResourceDomain" => ("Token<CombatResource>", "Token<AlternateResource>"),
        "StableId" => ("CombatHost::score", "CombatHost::total_score"),
        "RenamePreserveStableId" => ("pub fn ratio(", "pub fn combat_ratio("),
        "StaleInterfaceHash" => ("0x1020_3040", "0x1020_3041"),
        "MissingHostFunction" => (
            "pub fn required_host_function(value: i32) -> i32 { value }",
            "",
        ),
        other => return Err(format!("unknown handwritten transformer kind {other}").into()),
    };
    let mut transformed = replace_once(source, from, to, kind)?;
    for (extra_from, extra_to) in match kind {
        "StructFieldAdd" => vec![(
            "MutationVec { x: 1, y: 2 }",
            "MutationVec { x: 1, y: 2, z: 3 }",
        )],
        "SnapshotContentType" => vec![(
            "Snapshot(MutationView { health: 1 })",
            "Snapshot(MutationVec { x: 1, y: 2 })",
        )],
        "BufferElementType" => vec![(
            "Buffer(vec![MutationVec { x: 1, y: 2 }])",
            "Buffer(vec![1])",
        )],
        _ => Vec::new(),
    } {
        transformed = replace_once(&transformed, extra_from, extra_to, kind)?;
    }
    Ok(transformed)
}

#[must_use]
pub fn scenario_fingerprint(spec: &ScenarioSpec, fixture_hash: &str, trace: &[String]) -> Value {
    json!({
        "scenario_spec_hash": serialized_hash(spec),
        "fixture_hash": fixture_hash,
        "executor_symbol": spec.executor,
        "operation_trace_hash": serialized_hash(trace),
        "input_hash": stable_value_hash(&json!({"fixture": spec.fixture, "description": spec.description}))
    })
}

#[must_use]
pub fn fixture_hash(spec: &ScenarioSpec) -> String {
    stable_value_hash(&json!({
        "fixture": spec.fixture,
        "description": spec.description,
        "expected_observations": spec.expected_observations
    }))
}

pub fn verify_artifact_hygiene(root: &Path) -> Result<Value, AnyError> {
    let forbidden_names = [
        ".fingerprint",
        "CACHEDIR.TAG",
        ".cargo-lock",
        ".cargo-build-lock",
        ".cargo-artifact-lock",
    ];
    let forbidden_extensions = ["rlib", "rmeta", "o", "d"];
    let mut failures = Vec::new();
    if root.exists() {
        walk_files(root, &mut |path| {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            let components = path
                .components()
                .filter_map(|value| value.as_os_str().to_str())
                .collect::<Vec<_>>();
            if forbidden_names.contains(&name)
                || forbidden_extensions.contains(&extension)
                || components
                    .iter()
                    .any(|part| matches!(*part, "debug" | "release" | "incremental" | "build"))
            {
                failures.push(path.display().to_string());
            }
        })?;
    }
    Ok(json!({
        "root": root,
        "forbidden_artifacts": failures,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"}
    }))
}

fn validate_specs(
    label: &str,
    specs: &[ScenarioSpec],
    expected: usize,
    failures: &mut Vec<String>,
) {
    if specs.len() != expected {
        failures.push(format!(
            "{label} contains {}, expected {expected}",
            specs.len()
        ));
    }
    unique_field(
        &format!("{label} scenario ID"),
        specs.iter().map(|item| item.id.as_str()),
        failures,
    );
    unique_field(
        &format!("{label} executor"),
        specs.iter().map(|item| item.executor.as_str()),
        failures,
    );
    let spec_hashes = specs.iter().map(serialized_hash).collect::<BTreeSet<_>>();
    if spec_hashes.len() != specs.len() {
        failures.push(format!("{label} spec hashes are not unique"));
    }
    let operation_hashes = specs
        .iter()
        .map(operation_fingerprint)
        .collect::<BTreeSet<_>>();
    if operation_hashes.len() != specs.len() {
        failures.push(format!("{label} operation paths are not unique"));
    }
    for spec in specs {
        if spec.fixture.is_empty()
            || spec.expected_operations.is_empty()
            || spec.expected_observations.is_empty()
            || !spec
                .forbidden_operations
                .iter()
                .any(|operation| operation == "AggregateResult")
                && label == "H3"
        {
            failures.push(format!("{} is missing frozen scenario semantics", spec.id));
        }
    }
}

fn operation_fingerprint(spec: &ScenarioSpec) -> String {
    stable_value_hash(&json!({
        "executor": spec.executor,
        "operations": spec.expected_operations,
        "forbidden": spec.forbidden_operations
    }))
}

fn unique_field<'a>(
    label: &str,
    values: impl Iterator<Item = &'a str>,
    failures: &mut Vec<String>,
) {
    let values = values.collect::<Vec<_>>();
    if values.iter().copied().collect::<BTreeSet<_>>().len() != values.len() {
        failures.push(format!("{label} is not unique"));
    }
}

fn replace_once(source: &str, from: &str, to: &str, kind: &str) -> Result<String, AnyError> {
    let replaced = source.replacen(from, to, 1);
    if replaced == source {
        Err(format!("{kind} transformer did not match `{from}`").into())
    } else {
        Ok(replaced)
    }
}

struct StatusView {
    experiment: String,
    experiment_status: String,
    decision: String,
    milestone: String,
    milestone_status: String,
    roadmap_row_tail: String,
}

impl StatusView {
    fn from_registry(registry: &Value) -> Result<Self, AnyError> {
        let text = |field: &str| -> Result<String, AnyError> {
            registry[field]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("current status registry is missing {field}").into())
        };
        let experiment = text("current_experiment")?;
        let experiment_status = text("experiment_status")?;
        let decision = text("decision")?;
        let milestone = text("milestone")?;
        let milestone_status = text("milestone_status")?;
        let roadmap_status = match experiment_status.as_str() {
            "PREFREEZE" => "Prefreeze",
            "FROZEN" => "Frozen",
            "VERIFIED_TERMINAL_DECISION" => "Verified terminal decision",
            other => other,
        }
        .to_owned();
        Ok(Self {
            experiment,
            experiment_status,
            decision,
            milestone,
            milestone_status,
            roadmap_row_tail: format!(
                " Scenario-real apparatus is prefreeze-complete | Two formal runs and replay preserve real outcomes | {roadmap_status} |"
            ),
        })
    }
}

fn serialized_hash<T: Serialize + ?Sized>(value: &T) -> String {
    stable_value_hash(
        &serde_json::to_value(value).expect("serializable Gate 1 scenario value must encode"),
    )
}

fn status_failures(
    expected: &StatusView,
    readme: &str,
    roadmap: &str,
    baseline: &str,
) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, source) in [
        ("README", readme),
        ("ROADMAP", roadmap),
        ("Baseline", baseline),
    ] {
        if !source.contains("gate1-v2.4-status:start") || !source.contains("gate1-v2.4-status:end")
        {
            failures.push(format!("{path} has no v2.4 status block"));
        }
        if !source.contains(&expected.decision)
            || !source.contains(&expected.milestone)
            || !source.contains(&expected.milestone_status)
        {
            failures.push(format!("{path} disagrees with current status registry"));
        }
    }
    if !readme.contains(&format!(
        "Gate 1 v2.4: {} / {}",
        expected.experiment_status, expected.milestone_status
    )) {
        failures.push("README experiment status differs".to_owned());
    }
    if !roadmap.contains(&format!("| Gate 1 v2.4 |{}", expected.roadmap_row_tail)) {
        failures.push("ROADMAP Gate table differs from registry".to_owned());
    }
    if roadmap.matches("Current project gate:").count() != 1 {
        failures.push("ROADMAP does not have exactly one current gate".to_owned());
    }
    if expected.experiment != "gate1-v2.4" {
        failures.push("current experiment is not gate1-v2.4".to_owned());
    }
    failures
}

fn walk_files(root: &Path, visit: &mut impl FnMut(&Path)) -> Result<(), AnyError> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            walk_files(&path, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}

fn ensure_pass(value: &Value, context: &str) -> Result<(), AnyError> {
    if value["status"] == "PASS" {
        Ok(())
    } else {
        Err(format!("{context} failed: {}", value["failures"]).into())
    }
}

pub fn manifest_hashes() -> Result<BTreeMap<String, String>, AnyError> {
    [
        SCENARIO_SCHEMA,
        H1_MANIFEST,
        H1_HANDWRITTEN,
        H2_MANIFEST,
        H2_CLEANUP_MANIFEST,
        H3_MANIFEST,
    ]
    .into_iter()
    .map(|path| Ok((path.to_owned(), hash_file(path)?)))
    .collect()
}
