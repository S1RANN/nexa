#![no_main]

use libfuzzer_sys::fuzz_target;
use nexa_runtime::model_adapter::{RealmV5RuntimeAdapter, RealmV5RuntimeEvent};

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 128 {
        return;
    }
    let mut runtime = RealmV5RuntimeAdapter::new();
    for byte in bytes.iter().copied().take(64) {
        let event = match byte % 28 {
            0 => RealmV5RuntimeEvent::TaskAdmission,
            1 => RealmV5RuntimeEvent::PollTask,
            2 => RealmV5RuntimeEvent::FuelYield,
            3 => RealmV5RuntimeEvent::ExplicitYield,
            4 => RealmV5RuntimeEvent::ResumeTask,
            5 => RealmV5RuntimeEvent::TaskComplete,
            6 => RealmV5RuntimeEvent::HostWait,
            7 => RealmV5RuntimeEvent::HostComplete,
            8 => RealmV5RuntimeEvent::Cancel,
            9 => RealmV5RuntimeEvent::Cleanup,
            10 => RealmV5RuntimeEvent::BeginReload,
            11 => RealmV5RuntimeEvent::Quiesce,
            12 => RealmV5RuntimeEvent::Migration,
            13 => RealmV5RuntimeEvent::Rollback,
            14 => RealmV5RuntimeEvent::Commit,
            15 => RealmV5RuntimeEvent::ActivationFault,
            16 => RealmV5RuntimeEvent::LateCompletion,
            17 => RealmV5RuntimeEvent::TokenAcquire,
            18 => RealmV5RuntimeEvent::TokenRelease,
            19 => RealmV5RuntimeEvent::SnapshotAcquire,
            20 => RealmV5RuntimeEvent::SnapshotRelease,
            21 => RealmV5RuntimeEvent::ReleaseDrain,
            22 => RealmV5RuntimeEvent::GcRootAttach,
            23 => RealmV5RuntimeEvent::GcRootDrop,
            24 => RealmV5RuntimeEvent::GcCollect,
            25 => RealmV5RuntimeEvent::RetiredEpochReap(byte % 4),
            26 => RealmV5RuntimeEvent::RuntimeHostBeginClose,
            _ => RealmV5RuntimeEvent::RuntimeHostFinishClose,
        };
        let _ = runtime.apply(event);
        let _ = runtime.snapshot();
    }
});
