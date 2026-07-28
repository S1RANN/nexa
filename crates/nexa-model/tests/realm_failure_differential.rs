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
        let runtime_before = runtime.snapshot();
        assert!(model.apply(model_event).is_err());
        assert!(runtime.apply(runtime_event).is_err());
        assert_eq!(model.snapshot(), model_before);
        assert_eq!(runtime.snapshot(), runtime_before);
    }
}
