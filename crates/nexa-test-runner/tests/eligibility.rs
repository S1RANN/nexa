use nexa_test_runner::{CallGraph, CallGraphNode, EligibilityViolationReason, ForbiddenEffect};

#[test]
fn an_indirect_host_reachability_is_rejected_with_the_call_path() {
    let graph = CallGraph::new([
        CallGraphNode::new("test.entry").with_calls(["library.helper"]),
        CallGraphNode::new("library.helper").with_calls(["app.host_wrapper"]),
        CallGraphNode::new("app.host_wrapper").with_forbidden_effects([ForbiddenEffect::Host]),
    ])
    .unwrap();

    let report = graph.validate_tests(["test.entry"]);

    assert!(!report.is_eligible());
    assert_eq!(report.violations.len(), 1);
    assert_eq!(
        report.violations[0].path,
        ["test.entry", "library.helper", "app.host_wrapper"]
    );
    assert_eq!(
        report.violations[0].reason,
        EligibilityViolationReason::Forbidden(ForbiddenEffect::Host)
    );
}

#[test]
fn every_forbidden_effect_is_rejected() {
    let effects = [
        ForbiddenEffect::Host,
        ForbiddenEffect::Task,
        ForbiddenEffect::Await,
        ForbiddenEffect::Yield,
        ForbiddenEffect::Activation,
        ForbiddenEffect::Migration,
        ForbiddenEffect::PersistentState,
    ];
    let graph = CallGraph::new([
        CallGraphNode::new("test.entry").with_calls(["bad"]),
        CallGraphNode::new("bad").with_forbidden_effects(effects),
    ])
    .unwrap();

    let report = graph.validate_tests(["test.entry"]);
    let found: Vec<_> = report
        .violations
        .iter()
        .map(|violation| match violation.reason {
            EligibilityViolationReason::Forbidden(effect) => effect,
            EligibilityViolationReason::MissingMetadata => {
                panic!("all graph nodes have metadata")
            }
        })
        .collect();

    assert_eq!(found, effects);
}

#[test]
fn effect_free_recursive_graphs_terminate_and_are_eligible() {
    let graph = CallGraph::new([
        CallGraphNode::new("test.entry").with_calls(["helper"]),
        CallGraphNode::new("helper").with_calls(["test.entry"]),
    ])
    .unwrap();

    assert!(graph.validate_tests(["test.entry"]).is_eligible());
}

#[test]
fn missing_reachable_metadata_is_rejected() {
    let graph = CallGraph::new([CallGraphNode::new("test.entry").with_calls(["missing"])]).unwrap();

    let report = graph.validate_tests(["test.entry"]);

    assert_eq!(report.violations.len(), 1);
    assert_eq!(
        report.violations[0].reason,
        EligibilityViolationReason::MissingMetadata
    );
    assert_eq!(report.violations[0].path, ["test.entry", "missing"]);
}
