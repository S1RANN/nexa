use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use nexa_gate1_v2_6::{AnyError, read_json, stable_value_hash, write_json};

const FIXTURE_ROOT: &str = "experiments/gate1-v2.6/fixtures/v2_5_h2";
const COMPARISON_FIXTURE_ROOT: &str = "experiments/gate1-v2.6/fixtures/v2_5_comparisons";
const CHECK_ROOT: &str = "target/gate1-v2.6-dryrun";

const PRODUCTION_FIELDS: &[&str] = &[
    "calls_per_frame",
    "completed",
    "complex_types",
    "complex_value_count",
    "expected_calls",
    "expected_promotions",
    "first_slice_target_percent",
    "host_call",
    "host_call_count",
    "module_fingerprint",
    "observed_calls",
    "observed_first_slice",
    "observed_promotions",
    "peak_resources",
    "promotion_target_percent",
    "trace",
    "trace_event_count",
];

const FORBIDDEN_SEMANTIC_FIELDS: &[&str] = &[
    "elapsed_ns",
    "throughput_calls_per_second",
    "mean_ns",
    "p50_ns",
    "p90_ns",
    "p95_ns",
    "p99_ns",
    "min_ns",
    "max_ns",
    "standard_deviation_ns",
    "coefficient_of_variation",
    "frame_1000_calls_ns",
    "wall_clock_timestamp",
    "monotonic_timestamp",
    "pid",
    "parent_pid",
    "process_nonce",
    "one_time_token",
    "temporary_path",
    "worktree",
    "output_directory",
    "absolute_path",
    "event_sequence",
    "event_write_time",
    "event_log_hash",
    "process_attestation_hash",
    "run_id",
    "provenance",
];

pub fn h2_semantic_projection(result: &Value) -> Result<Value, AnyError> {
    let semantic = result
        .get("semantic")
        .ok_or("H2 result has no semantic payload")?;
    let production_cases = array_at(semantic, "/production_matrix/cases")?
        .iter()
        .map(|case| select_fields(case, PRODUCTION_FIELDS))
        .collect::<Result<Vec<_>, _>>()?;
    let scenarios = array_at(semantic, "/snapshot_scenarios")?
        .iter()
        .map(project_scenario)
        .collect::<Result<Vec<_>, _>>()?;
    let cleanup = array_at(semantic, "/cleanup_matrix")?
        .iter()
        .map(project_cleanup)
        .collect::<Vec<_>>();
    let projection = json!({
        "schema_version": 1,
        "projection": "gate1-v2.6-h2-semantic-v1",
        "production_cases": production_cases,
        "scenarios": scenarios,
        "cleanup": cleanup
    });
    ensure_no_forbidden_fields(&projection)?;
    Ok(projection)
}

pub fn h2_semantic_signature(result: &Value) -> Result<String, AnyError> {
    Ok(stable_value_hash(&h2_semantic_projection(result)?))
}

pub fn h2_performance_projection(result: &Value) -> Result<Value, AnyError> {
    let semantic = result
        .get("semantic")
        .ok_or("H2 result has no semantic payload")?;
    let matrix_cases = array_at(semantic, "/production_matrix/cases")?
        .iter()
        .map(|case| {
            select_fields(
                case,
                &[
                    "calls_per_frame",
                    "first_slice_target_percent",
                    "trace",
                    "host_call",
                    "complex_types",
                    "elapsed_ns",
                    "throughput_calls_per_second",
                ],
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let benchmark_processes = result
        .get("performance_processes")
        .and_then(Value::as_array)
        .ok_or("H2 result has no performance_processes")?
        .iter()
        .enumerate()
        .map(|(index, process)| {
            let cases = process
                .get("cases")
                .and_then(Value::as_array)
                .ok_or("benchmark process has no cases")?
                .iter()
                .map(|case| {
                    select_fields(
                        case,
                        &[
                            "case",
                            "mean_ns",
                            "p50_ns",
                            "p90_ns",
                            "p95_ns",
                            "p99_ns",
                            "min_ns",
                            "max_ns",
                            "standard_deviation_ns",
                            "coefficient_of_variation",
                            "throughput_ops_per_second",
                            "frame_1000_calls_ns",
                            "samples",
                        ],
                    )
                })
                .collect::<Result<Vec<_>, AnyError>>()?;
            Ok(json!({
                "process_index": index + 1,
                "samples": process["samples"],
                "cases": cases
            }))
        })
        .collect::<Result<Vec<_>, AnyError>>()?;
    Ok(json!({
        "schema_version": 1,
        "projection": "gate1-v2.6-h2-performance-v1",
        "warmup_samples": 100,
        "timed_samples": 1000,
        "production_matrix": matrix_cases,
        "benchmark_processes": benchmark_processes
    }))
}

pub fn h2_allocation_projection(result: &Value) -> Result<Value, AnyError> {
    let observer = result
        .get("allocation_observer")
        .ok_or("H2 result has no allocation observer")?;
    let runs = observer
        .get("runs")
        .and_then(Value::as_array)
        .ok_or("allocation observer has no runs")?;
    let selected = [
        "promotion",
        "fuel_resume",
        "explicit_resume",
        "host_resume",
        "trace_off",
    ];
    let projected_runs = runs
        .iter()
        .map(|run| select_fields(run, &selected))
        .collect::<Result<Vec<_>, _>>()?;
    let observer_run_count = projected_runs.len();
    Ok(json!({
        "schema_version": 1,
        "projection": "gate1-v2.6-h2-allocation-v1",
        "observer_run_count": observer_run_count,
        "baseline_subtracted": true,
        "runs": projected_runs
    }))
}

pub fn compare_h2(left: &Value, right: &Value) -> Result<Value, AnyError> {
    let left_semantic = h2_semantic_projection(left)?;
    let right_semantic = h2_semantic_projection(right)?;
    let semantic_equal = left_semantic == right_semantic;
    let left_allocation = h2_allocation_projection(left)?;
    let right_allocation = h2_allocation_projection(right)?;
    let allocation_equal = left_allocation == right_allocation;
    let performance = compare_performance(
        &h2_performance_projection(left)?,
        &h2_performance_projection(right)?,
    )?;
    let performance_status = performance["status"].as_str().unwrap_or("INVALID");
    let status = if !semantic_equal || !allocation_equal {
        "FAIL"
    } else if performance_status == "INVALID" {
        "INVALID"
    } else if performance_status == "INCONCLUSIVE" {
        "INCONCLUSIVE"
    } else {
        "PASS"
    };
    Ok(json!({
        "status": status,
        "semantic": {
            "status": if semantic_equal {"PASS"} else {"FAIL"},
            "failures": if semantic_equal {Vec::<String>::new()} else {vec!["semantic projections differ".to_owned()]},
            "observations": {
                "equal": semantic_equal,
                "left_signature": stable_value_hash(&left_semantic),
                "right_signature": stable_value_hash(&right_semantic)
            },
            "rule": "equal outcomes, semantic signatures, 32 scenario projections, 12 cleanup projections, and runtime invariant classes"
        },
        "allocation": {
            "status": if allocation_equal {"PASS"} else {"FAIL"},
            "failures": if allocation_equal {Vec::<String>::new()} else {vec!["allocation projections differ".to_owned()]},
            "observations": {
                "equal": allocation_equal,
                "left": left_allocation,
                "right": right_allocation
            },
            "rule": "equal allocation projection and equal absolute zero-allocation conditions"
        },
        "performance": performance
    }))
}

pub fn comparison_truth_table() -> Result<Value, AnyError> {
    let cases = [
        ("all-pass", "PASS", "PASS", "PASS", "PASS", "PASS"),
        (
            "performance-inconclusive",
            "PASS",
            "PASS",
            "INCONCLUSIVE",
            "INCONCLUSIVE",
            "PASS",
        ),
        ("semantic-fail", "FAIL", "PASS", "PASS", "FAIL", "PASS"),
        ("allocation-fail", "PASS", "FAIL", "PASS", "FAIL", "PASS"),
        (
            "artifact-invalid",
            "INVALID",
            "PASS",
            "PASS",
            "INVALID",
            "PASS",
        ),
        (
            "wrong-inconclusive-as-fail",
            "PASS",
            "PASS",
            "INCONCLUSIVE",
            "FAIL",
            "FAIL",
        ),
    ];
    let records = cases
        .into_iter()
        .map(
            |(name, semantic, allocation, performance, recorded, expected_contract)| {
                let derived = derive_comparison_outcome(semantic, allocation, performance);
                let contract =
                    comparison_contract_status(recorded, semantic, allocation, performance, true);
                json!({
                    "case": name,
                    "components": {
                        "semantic": semantic,
                        "allocation": allocation,
                        "performance": performance
                    },
                    "derived_outcome": derived,
                    "recorded_outcome": recorded,
                    "contract_status": contract,
                    "matched": contract == expected_contract
                })
            },
        )
        .collect::<Vec<_>>();
    let failures = records
        .iter()
        .filter(|record| record["matched"] != true)
        .map(|record| format!("{} truth-table mismatch", record["case"]))
        .collect::<Vec<_>>();
    check_result("comparison_truth_table", &failures, &json!(records))
}

pub fn inconclusive_contract_check() -> Result<Value, AnyError> {
    let formal = read_json(format!("{COMPARISON_FIXTURE_ROOT}/formal.json"))?;
    let replay = read_json(format!("{COMPARISON_FIXTURE_ROOT}/replay.json"))?;
    let mut failures = Vec::new();
    let mut records = Vec::new();
    for (name, fixture) in [("formal", formal), ("replay", replay)] {
        let semantic = fixture
            .pointer("/h2/semantic/status")
            .and_then(Value::as_str)
            .unwrap_or("INVALID");
        let allocation = fixture
            .pointer("/h2/allocation/status")
            .and_then(Value::as_str)
            .unwrap_or("INVALID");
        let performance = fixture
            .pointer("/h2/performance/status")
            .and_then(Value::as_str)
            .unwrap_or("INVALID");
        let recorded = fixture["status"].as_str().unwrap_or("INVALID");
        let contract =
            comparison_contract_status(recorded, semantic, allocation, performance, true);
        let passed = semantic == "PASS"
            && allocation == "PASS"
            && performance == "INCONCLUSIVE"
            && recorded == "INCONCLUSIVE"
            && contract == "PASS"
            && fixture["formal_evidence_usable"] == false;
        if !passed {
            failures.push(format!(
                "{name} legal INCONCLUSIVE fixture did not map to PASS"
            ));
        }
        records.push(json!({
            "fixture": name,
            "formal_evidence_usable": fixture["formal_evidence_usable"],
            "components": {
                "semantic": semantic,
                "allocation": allocation,
                "performance": performance
            },
            "outcome": recorded,
            "contract_status": contract
        }));
    }
    for (name, semantic, allocation, performance, recorded, schema_valid) in [
        (
            "semantic-diff-mislabeled",
            "FAIL",
            "PASS",
            "INCONCLUSIVE",
            "INCONCLUSIVE",
            true,
        ),
        (
            "allocation-diff-mislabeled",
            "PASS",
            "FAIL",
            "INCONCLUSIVE",
            "INCONCLUSIVE",
            true,
        ),
        (
            "performance-within-tolerance-mislabeled",
            "PASS",
            "PASS",
            "PASS",
            "INCONCLUSIVE",
            true,
        ),
        (
            "missing-performance-samples",
            "PASS",
            "PASS",
            "INVALID",
            "INCONCLUSIVE",
            false,
        ),
        (
            "components-outcome-mismatch",
            "PASS",
            "PASS",
            "INCONCLUSIVE",
            "PASS",
            true,
        ),
    ] {
        let contract =
            comparison_contract_status(recorded, semantic, allocation, performance, schema_valid);
        if contract != "FAIL" {
            failures.push(format!("{name} was not rejected"));
        }
        records.push(json!({"negative": name, "contract_status": contract}));
    }
    check_result("inconclusive_contract", &failures, &json!(records))
}

pub fn derive_comparison_outcome(
    semantic: &str,
    allocation: &str,
    performance: &str,
) -> &'static str {
    if semantic == "INVALID" || allocation == "INVALID" || performance == "INVALID" {
        "INVALID"
    } else if semantic == "FAIL" || allocation == "FAIL" {
        "FAIL"
    } else if semantic == "PASS" && allocation == "PASS" && performance == "INCONCLUSIVE" {
        "INCONCLUSIVE"
    } else if semantic == "PASS" && allocation == "PASS" && performance == "PASS" {
        "PASS"
    } else {
        "INVALID"
    }
}

pub fn comparison_contract_status(
    recorded: &str,
    semantic: &str,
    allocation: &str,
    performance: &str,
    schema_and_provenance_valid: bool,
) -> &'static str {
    if schema_and_provenance_valid
        && recorded == derive_comparison_outcome(semantic, allocation, performance)
    {
        "PASS"
    } else {
        "FAIL"
    }
}

pub fn forbidden_semantic_fields() -> &'static [&'static str] {
    FORBIDDEN_SEMANTIC_FIELDS
}

pub fn projection_check() -> Result<Value, AnyError> {
    let fixtures = fixtures()?;
    let mut failures = Vec::new();
    let mut observations = Vec::new();
    for (run, result) in fixtures {
        let projection = h2_semantic_projection(&result)?;
        let production = projection["production_cases"]
            .as_array()
            .map_or(0, Vec::len);
        let scenarios = projection["scenarios"].as_array().map_or(0, Vec::len);
        let cleanup = projection["cleanup"].as_array().map_or(0, Vec::len);
        if production != 32 || scenarios != 32 || cleanup != 12 {
            failures.push(format!(
                "{run} projection has {production}/{scenarios}/{cleanup} records"
            ));
        }
        ensure_no_forbidden_fields(&projection)?;
        observations.push(json!({
            "run": run,
            "production_cases": production,
            "scenarios": scenarios,
            "cleanup_triggers": cleanup,
            "forbidden_field_count": forbidden_semantic_fields().len(),
            "signature": stable_value_hash(&projection)
        }));
    }
    check_result("h2_projection", &failures, &json!(observations))
}

pub fn noise_invariance_check() -> Result<Value, AnyError> {
    let fixtures = fixtures()?;
    let projections = fixtures
        .iter()
        .map(|(_, result)| h2_semantic_projection(result))
        .collect::<Result<Vec<_>, _>>()?;
    let signatures = projections
        .iter()
        .map(stable_value_hash)
        .collect::<Vec<_>>();
    let performance = fixtures
        .iter()
        .map(|(_, result)| h2_performance_projection(result))
        .collect::<Result<Vec<_>, _>>()?;
    let old_signatures = fixtures
        .iter()
        .map(|(_, result)| result["semantic_signature"].clone())
        .collect::<Vec<_>>();
    let mut perturbed = fixtures[0].1.clone();
    perturb_noise(&mut perturbed);
    let perturbed_semantic = h2_semantic_projection(&perturbed)?;
    let perturbed_performance = h2_performance_projection(&perturbed)?;
    let mut failures = Vec::new();
    if !all_equal(&projections) {
        failures.push("v2.5 fixtures do not have identical semantic projections".to_owned());
    }
    if !all_equal(&signatures) {
        failures.push("v2.5 fixtures do not have identical semantic signatures".to_owned());
    }
    if all_equal(&performance) {
        failures.push("v2.5 performance projections unexpectedly match".to_owned());
    }
    if perturbed_semantic != projections[0] {
        failures.push("synthetic timing/process noise changed the semantic projection".to_owned());
    }
    if perturbed_performance == performance[0] {
        failures
            .push("synthetic timing noise did not change the performance projection".to_owned());
    }
    check_result(
        "h2_noise_invariance",
        &failures,
        &json!({
            "fixture_signatures": signatures,
            "recorded_v2_5_signatures": old_signatures,
            "performance_projections_differ": !all_equal(&performance),
            "synthetic_noise_semantic_invariant": perturbed_semantic == projections[0],
            "synthetic_noise_performance_changed": perturbed_performance != performance[0]
        }),
    )
}

pub fn semantic_sensitivity_check() -> Result<Value, AnyError> {
    let fixture = read_json(format!("{FIXTURE_ROOT}/formal-run-1.json"))?;
    let baseline = h2_semantic_signature(&fixture)?;
    let mutations = [
        (
            "observed-task-count",
            "/semantic/snapshot_scenarios/0/observed_calls",
        ),
        (
            "observed-promotion-count",
            "/semantic/snapshot_scenarios/0/observed_promotions",
        ),
        (
            "host-call-count",
            "/semantic/snapshot_scenarios/0/host_call_count",
        ),
        (
            "trace-enabled",
            "/semantic/snapshot_scenarios/0/dimensions/trace",
        ),
        (
            "value-shape",
            "/semantic/snapshot_scenarios/0/dimensions/value_shape",
        ),
        (
            "final-ledger",
            "/semantic/snapshot_scenarios/0/cohorts/0/after/resources/tasks",
        ),
        ("cleanup-trigger", "/semantic/cleanup_matrix/0/id"),
        ("cleanup-outcome", "/semantic/cleanup_matrix/0/status"),
        (
            "cohort-outcome",
            "/semantic/snapshot_scenarios/0/cohorts/0/operation_results/0/result",
        ),
    ];
    let mut cases = Vec::new();
    let mut failures = Vec::new();
    for (name, pointer) in mutations {
        let mut changed = fixture.clone();
        let target = changed
            .pointer_mut(pointer)
            .ok_or_else(|| format!("sensitivity pointer is missing: {pointer}"))?;
        *target = changed_value(target);
        let signature = h2_semantic_signature(&changed)?;
        let sensitive = signature != baseline;
        if !sensitive {
            failures.push(format!("{name} did not change the semantic signature"));
        }
        cases.push(json!({
            "mutation": name,
            "pointer": pointer,
            "signature_changed": sensitive
        }));
    }
    let mut violations = fixture.clone();
    violations
        .pointer_mut("/semantic/snapshot_scenarios/0/violations")
        .and_then(Value::as_array_mut)
        .ok_or("scenario violations are missing")?
        .push(json!("synthetic-invariant-violation"));
    let violation_changed = h2_semantic_signature(&violations)? != baseline;
    if !violation_changed {
        failures.push("invariant violation did not change the semantic signature".to_owned());
    }
    cases.push(json!({
        "mutation": "invariant-violations",
        "pointer": "/semantic/snapshot_scenarios/0/violations",
        "signature_changed": violation_changed
    }));
    check_result("h2_semantic_sensitivity", &failures, &json!(cases))
}

pub fn comparison_policy_check() -> Result<Value, AnyError> {
    let fixtures = fixtures()?;
    let formal = compare_h2(&fixtures[0].1, &fixtures[1].1)?;
    let replay = compare_h2(&fixtures[0].1, &fixtures[2].1)?;
    let mut semantic_change = fixtures[1].1.clone();
    let target = semantic_change
        .pointer_mut("/semantic/snapshot_scenarios/0/observed_calls")
        .ok_or("comparison sensitivity field is missing")?;
    *target = changed_value(target);
    let semantic_failure = compare_h2(&fixtures[0].1, &semantic_change)?;
    let mut performance_change = fixtures[1].1.clone();
    for process in 0..3 {
        let pointer = format!("/performance_processes/{process}/cases/0/p95_ns");
        let target = performance_change
            .pointer_mut(&pointer)
            .ok_or("comparison performance field is missing")?;
        *target = json!(1_000_000_000_u64);
    }
    let performance_inconclusive = compare_h2(&fixtures[0].1, &performance_change)?;
    let mut failures = Vec::new();
    if formal["status"] != "INCONCLUSIVE" {
        failures.push("stable v2.5 Formal H2 FAIL results do not compare INCONCLUSIVE".to_owned());
    }
    if replay["status"] != "INCONCLUSIVE" {
        failures.push("stable v2.5 Replay H2 FAIL result does not compare INCONCLUSIVE".to_owned());
    }
    if semantic_failure["status"] != "FAIL" {
        failures.push("semantic mutation is not classified FAIL".to_owned());
    }
    if performance_inconclusive["status"] != "INCONCLUSIVE" {
        failures.push("performance-only excess is not classified INCONCLUSIVE".to_owned());
    }
    check_result(
        "comparison_policy",
        &failures,
        &json!({
            "formal": formal["status"],
            "replay": replay["status"],
            "semantic_change": semantic_failure["status"],
            "performance_only": performance_inconclusive["status"]
        }),
    )
}

fn project_scenario(scenario: &Value) -> Result<Value, AnyError> {
    let dimensions = scenario
        .get("dimensions")
        .cloned()
        .ok_or("H2 scenario has no dimensions")?;
    let scenario_id = scenario["scenario"]
        .as_str()
        .ok_or("H2 scenario has no scenario ID")?;
    let cohorts = scenario
        .get("cohorts")
        .and_then(Value::as_array)
        .ok_or("H2 scenario has no cohorts")?
        .iter()
        .map(|cohort| {
            let after = cohort
                .get("after")
                .ok_or("H2 cohort has no final snapshot")?;
            let terminal_state_counts = after
                .get("tasks")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .filter_map(|task| task["state"].as_str())
                .fold(BTreeMap::<String, u64>::new(), |mut counts, state| {
                    *counts.entry(state.to_owned()).or_default() += 1;
                    counts
                });
            Ok(json!({
                "cohort": cohort["cohort"],
                "operation_results": cohort["operation_results"],
                "terminal_state_counts": terminal_state_counts,
                "final_resource_ledger": after["resources"],
                "final_completion_accounting": after["completion_accounting"],
                "final_release_queue_count": after["release_queue"].as_array().map_or(0, Vec::len),
                "final_request_reservations": after.pointer("/resources/requests"),
                "final_completion_reservations": after.pointer("/resources/completion_reservations"),
                "violations": cohort["violations"]
            }))
        })
        .collect::<Result<Vec<_>, AnyError>>()?;
    let production = select_fields(
        scenario
            .get("production")
            .ok_or("H2 scenario has no production observation")?,
        PRODUCTION_FIELDS,
    )?;
    Ok(json!({
        "scenario_id": scenario_id,
        "scenario_spec_hash": stable_value_hash(&dimensions),
        "configuration_dimensions": dimensions,
        "expected_task_count": scenario["expected_calls"],
        "observed_task_count": scenario["observed_calls"],
        "expected_promotion_count": scenario["expected_promotions"],
        "observed_promotion_count": scenario["observed_promotions"],
        "host_call_count": scenario["host_call_count"],
        "trace_event_count": scenario["trace_event_count"],
        "complex_value_count": scenario["complex_value_count"],
        "execution_fingerprint": scenario["execution_fingerprint"],
        "production": production,
        "cohorts": cohorts,
        "invariant_violation_kinds": scenario["violations"]
    }))
}

fn project_cleanup(case: &Value) -> Value {
    let terminal_state_counts = case
        .get("terminal")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|task| task["state"].as_str())
        .fold(BTreeMap::<String, u64>::new(), |mut counts, state| {
            *counts.entry(state.to_owned()).or_default() += 1;
            counts
        });
    json!({
        "cleanup_trigger_id": case["id"],
        "cleanup_trigger_fingerprint": case["trigger_fingerprint"],
        "executor": case["executor"],
        "expected_operations": case["expected_operations"],
        "operation_trace": case["operation_trace"],
        "cleanup_terminal_outcome": case["status"],
        "observation_passed": case["observation_passed"],
        "terminal_state_counts": terminal_state_counts,
        "final_resource_ledger": case["ledger"],
        "final_request_reservations": case["request"],
        "final_completion_reservations": case["completion"],
        "final_release_queue_count": case["release"],
        "capacity_rejected": case["capacity_rejected"],
        "panic_contained": case["panic_contained"]
    })
}

fn array_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a Vec<Value>, AnyError> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array at {pointer}").into())
}

fn select_fields(value: &Value, fields: &[&str]) -> Result<Value, AnyError> {
    let object = value
        .as_object()
        .ok_or("projection input is not an object")?;
    let mut selected = Map::new();
    for field in fields {
        selected.insert(
            (*field).to_owned(),
            object
                .get(*field)
                .cloned()
                .ok_or_else(|| format!("projection field {field} is missing"))?,
        );
    }
    Ok(Value::Object(selected))
}

fn ensure_no_forbidden_fields(value: &Value) -> Result<(), AnyError> {
    let serialized = serde_json::to_string(value)?;
    for field in FORBIDDEN_SEMANTIC_FIELDS {
        if serialized.contains(&format!("\"{field}\"")) {
            return Err(format!("forbidden semantic field {field} is present").into());
        }
    }
    Ok(())
}

fn compare_performance(left: &Value, right: &Value) -> Result<Value, AnyError> {
    let policy = read_json("experiments/gate1-v2.6/h2_performance_policy.json")?;
    let tolerances = policy
        .pointer("/cross_run_tolerance")
        .and_then(Value::as_object)
        .ok_or("performance policy has no tolerances")?;
    let left_cases = aggregate_benchmark_cases(left)?;
    let right_cases = aggregate_benchmark_cases(right)?;
    if left_cases.keys().collect::<Vec<_>>() != right_cases.keys().collect::<Vec<_>>() {
        return Ok(json!({
            "status": "INVALID",
            "failures": ["benchmark case sets differ"],
            "observations": {"comparisons": []},
            "rule": "three valid benchmark processes per run, absolute budgets unchanged, frozen cross-run tolerance"
        }));
    }
    let mut comparisons = Vec::new();
    let mut outside = Vec::new();
    for (case, left_metrics) in &left_cases {
        let right_metrics = right_cases
            .get(case)
            .ok_or("right benchmark case is missing")?;
        for metric in [
            "mean_ns",
            "p50_ns",
            "p90_ns",
            "p95_ns",
            "p99_ns",
            "throughput_ops_per_second",
            "frame_1000_calls_ns",
        ] {
            let left_value = left_metrics[metric];
            let right_value = right_metrics[metric];
            let delta = relative_delta(left_value, right_value);
            let tolerance = tolerances[metric]
                .as_f64()
                .ok_or_else(|| format!("performance tolerance {metric} is missing"))?;
            let within = delta <= tolerance;
            if !within {
                outside.push(format!("{case}:{metric}"));
            }
            comparisons.push(json!({
                "case": case,
                "metric": metric,
                "left": left_value,
                "right": right_value,
                "delta": delta,
                "tolerance": tolerance,
                "within_tolerance": within
            }));
        }
        let left_cv = left_metrics["coefficient_of_variation"];
        let right_cv = right_metrics["coefficient_of_variation"];
        let delta = (left_cv - right_cv).abs();
        let tolerance = tolerances["coefficient_of_variation_absolute_delta"]
            .as_f64()
            .ok_or("CV absolute tolerance is missing")?;
        let within = delta <= tolerance;
        if !within {
            outside.push(format!("{case}:coefficient_of_variation"));
        }
        comparisons.push(json!({
            "case": case,
            "metric": "coefficient_of_variation",
            "left": left_cv,
            "right": right_cv,
            "delta": delta,
            "tolerance": tolerance,
            "within_tolerance": within
        }));
    }
    Ok(json!({
        "status": if outside.is_empty() {"PASS"} else {"INCONCLUSIVE"},
        "failures": outside,
        "observations": {
            "comparisons": comparisons,
            "aggregation": policy["aggregation"]
        },
        "rule": "absolute performance budgets are hypothesis outcomes; valid cross-run excess is INCONCLUSIVE"
    }))
}

fn aggregate_benchmark_cases(
    projection: &Value,
) -> Result<BTreeMap<String, BTreeMap<String, f64>>, AnyError> {
    let processes = projection
        .get("benchmark_processes")
        .and_then(Value::as_array)
        .ok_or("performance projection has no benchmark processes")?;
    if processes.len() != 3 {
        return Err(format!(
            "expected 3 benchmark processes, observed {}",
            processes.len()
        )
        .into());
    }
    let mut values = BTreeMap::<String, BTreeMap<String, Vec<f64>>>::new();
    for process in processes {
        if process["samples"] != 1000 {
            return Err("benchmark process does not contain 1000 samples".into());
        }
        for case in process["cases"]
            .as_array()
            .ok_or("benchmark process has no cases")?
        {
            let name = case["case"]
                .as_str()
                .ok_or("benchmark case has no name")?
                .to_owned();
            for metric in [
                "mean_ns",
                "p50_ns",
                "p90_ns",
                "p95_ns",
                "p99_ns",
                "throughput_ops_per_second",
                "frame_1000_calls_ns",
                "coefficient_of_variation",
            ] {
                let value = case[metric]
                    .as_f64()
                    .ok_or_else(|| format!("{name}:{metric} is missing or non-numeric"))?;
                if !value.is_finite() {
                    return Err(format!("{name}:{metric} is not finite").into());
                }
                values
                    .entry(name.clone())
                    .or_default()
                    .entry(metric.to_owned())
                    .or_default()
                    .push(value);
            }
        }
    }
    values
        .into_iter()
        .map(|(case, metrics)| {
            let aggregated = metrics
                .into_iter()
                .map(|(metric, mut samples)| {
                    samples.sort_by(f64::total_cmp);
                    if samples.len() != 3 {
                        return Err(format!("{case}:{metric} has {} samples", samples.len()).into());
                    }
                    Ok((metric, samples[1]))
                })
                .collect::<Result<BTreeMap<_, _>, AnyError>>()?;
            Ok((case, aggregated))
        })
        .collect()
}

fn relative_delta(left: f64, right: f64) -> f64 {
    let denominator = left.min(right).abs();
    if denominator <= f64::EPSILON {
        if (left - right).abs() <= f64::EPSILON {
            0.0
        } else {
            f64::INFINITY
        }
    } else {
        (left - right).abs() / denominator
    }
}

fn fixtures() -> Result<Vec<(String, Value)>, AnyError> {
    ["formal-run-1", "formal-run-2", "replay"]
        .into_iter()
        .map(|run| {
            Ok((
                run.to_owned(),
                read_json(format!("{FIXTURE_ROOT}/{run}.json"))?,
            ))
        })
        .collect()
}

fn perturb_noise(result: &mut Value) {
    for pointer in [
        "/semantic/production_matrix/cases/0/elapsed_ns",
        "/semantic/production_matrix/cases/0/throughput_calls_per_second",
        "/semantic/snapshot_scenarios/0/production/elapsed_ns",
        "/semantic/snapshot_scenarios/0/production/throughput_calls_per_second",
        "/performance_processes/0/cases/0/mean_ns",
        "/performance_processes/0/cases/0/p95_ns",
        "/performance_processes/0/cases/0/coefficient_of_variation",
        "/process/pid",
        "/process/process_nonce",
    ] {
        if let Some(value) = result.pointer_mut(pointer) {
            *value = changed_value(value);
        }
    }
    result["worker_pid"] = json!(999_999);
    result["worker_nonce"] = json!("synthetic-noise");
    result["event_log_hash"] = json!("synthetic-noise");
    result["process_attestation_hash"] = json!("synthetic-noise");
}

fn changed_value(value: &Value) -> Value {
    match value {
        Value::Bool(value) => json!(!value),
        Value::Number(value) => value
            .as_u64()
            .map_or_else(|| json!(123_456.75), |number| json!(number + 1)),
        Value::String(value) => json!(format!("{value}-changed")),
        Value::Null | Value::Array(_) | Value::Object(_) => json!("changed"),
    }
}

fn all_equal<T: PartialEq>(values: &[T]) -> bool {
    values
        .first()
        .is_none_or(|first| values.iter().all(|value| value == first))
}

fn check_result(name: &str, failures: &[String], metrics: &Value) -> Result<Value, AnyError> {
    let result = json!({
        "schema_version": 1,
        "experiment_version": "gate1-v2.6",
        "check": name,
        "status": if failures.is_empty() {"PASS"} else {"FAIL"},
        "failures": failures,
        "metrics": metrics
    });
    write_json(
        &std::path::Path::new(CHECK_ROOT).join(format!("{name}.json")),
        &result,
    )?;
    if result["status"] != "PASS" {
        return Err(format!("{name} failed: {}", result["failures"]).into());
    }
    Ok(result)
}
