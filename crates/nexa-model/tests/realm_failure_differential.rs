use nexa_model::realm::{RealmEvent, RealmModel};
use nexa_runtime::model_adapter::{RealmRuntimeModelAdapter, RuntimeRealmEvent};

#[test]
fn rejected_events_do_not_mutate_either_model() {
    let cases = [
        (RealmEvent::Poll, RuntimeRealmEvent::Poll),
        (
            RealmEvent::CompleteRequest,
            RuntimeRealmEvent::CompleteRequest,
        ),
        (
            RealmEvent::LateCompletion,
            RuntimeRealmEvent::LateCompletion,
        ),
    ];
    for (model_event, runtime_event) in cases {
        let mut model = RealmModel::default();
        let mut runtime = RealmRuntimeModelAdapter::default();
        let model_before = model.snapshot();
        let runtime_before = runtime.state_fingerprint();
        let counters_before = runtime.invocation_counters();
        assert!(model.apply(model_event).is_err());
        assert!(runtime.apply(runtime_event).is_err());
        assert_eq!(model.snapshot(), model_before);
        assert_eq!(runtime.state_fingerprint(), runtime_before);
        let attempted = !matches!(runtime_event, RuntimeRealmEvent::LateCompletion);
        assert_eq!(
            runtime.invocation_counters().total(),
            counters_before.total() + u64::from(attempted)
        );
    }
}
