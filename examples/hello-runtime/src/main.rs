//! Minimal Nexa onboarding example: one generated Host function printing to
//! stdout through the public `spawn_task -> poll_task` lifecycle.

use nexa_core::StableId;
use nexa_runtime::{
    RealmConfig, RealmRuntime, ResourceContext, RuntimeHost, RuntimeValue, StepConfig, TaskLimits,
    TaskPoll,
};

#[allow(dead_code)]
// Generated bindings keep an explicit lifetime on borrowed `string` arguments.
#[allow(clippy::needless_lifetimes)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/hello_api.rs"));
}

struct StdoutConsole;

impl generated::ConsoleHost for StdoutConsole {
    fn log(
        &mut self,
        _: &mut ResourceContext<'_>,
        message: &str,
    ) -> Result<i32, generated::HostError> {
        println!("{message}");
        i32::try_from(message.len())
            .map_err(|_| generated::HostError("message length exceeds i32".into()))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let idl = nexa_idl::parse(include_str!("../hello_api.nidl"))?;
    let host_hash = generated::INTERFACE_HASH;
    assert_eq!(host_hash, nexa_idl::exact_hash(&idl));
    let schema_hash = StableId::from_name("hello-state-v1");
    let verified =
        nexa_compiler::compile_with_interface(include_str!("../hello.nexa"), &idl, schema_hash)?;

    let runtime_host = RuntimeHost::new(16);
    let registry = generated::GeneratedHostRegistry::new(StdoutConsole);
    let mut realm = RealmRuntime::hosted(
        RealmConfig::default(),
        runtime_host.clone(),
        Box::new(registry),
    )?;
    let module = realm.load_module(verified, host_hash, schema_hash)?;

    let scope = realm.create_scope(None)?;
    let task = realm.spawn_task(
        module,
        0,
        &[],
        StepConfig {
            owner: scope,
            priority: 1,
            fuel_slice: 256,
            cumulative_budget: 1_024,
            limits: TaskLimits::default(),
        },
    )?;
    let TaskPoll::Completed(RuntimeValue::I32(written)) = realm.poll_task(task, 256)? else {
        return Err("hello task did not complete in one poll".into());
    };
    assert_eq!(written, 12, "console.log returned the written byte count");

    drop(realm);
    let _ = runtime_host.begin_close();
    runtime_host.try_finish_close()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn hello_task_completes_through_public_lifecycle() {
        super::run().expect("hello-runtime lifecycle");
    }
}
