//! Minimal high-level Nexa embedding example.

use std::sync::Arc;

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilitySet, MemoryPackage, MemorySource, NexaEngine,
    PackageId, PackagePolicy, PackageRuntimeLimits, SourceId, SourceIdentity, TrustLevel,
};

#[allow(dead_code)]
#[allow(clippy::needless_lifetimes)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/hello_api.rs"));
}

struct StdoutConsole;

impl generated::ConsoleHost for StdoutConsole {
    fn log(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        message: &str,
    ) -> Result<i32, generated::HostError> {
        println!("{message}");
        i32::try_from(message.len())
            .map_err(|_| generated::HostError("message length exceeds i32".into()))
    }
}

fn policy() -> PackagePolicy {
    PackagePolicy {
        trust: TrustLevel::FirstParty,
        capability_ceiling: CapabilitySet::default(),
        allowed_activation: ActivationSet::new([ActivationPolicy::Required]),
        max_packages: 1,
        runtime_limits: PackageRuntimeLimits::default(),
        allow_entitlement: false,
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(generated::Main::NAME, "main");
    let source = MemorySource::new(SourceId::new("hello")?, policy()).package(
        MemoryPackage::new(
            "hello",
            "schema=2\nkind='application'\nid='example.hello'\nname='Hello'\nversion='1.0.0'\n\
             source_root='src'\nentry='examples.hello'\nactivation='required'\npriority=0\n\
             capabilities=[]\nhandler_fuel=1024\n",
        )
        .source("src/examples/hello.nexa", include_str!("../hello.nexa")),
    );
    let mut engine = NexaEngine::builder(generated::contract())
        .host_contract_source(
            SourceIdentity::standalone("examples/hello-runtime/hello_api.nidl"),
            Arc::<str>::from(include_str!("../hello_api.nidl")),
        )
        .host_factory(|_: &nexa_embed::PackageContext| generated::registry(StdoutConsole))
        .package_source(source)
        .require_export::<generated::Main>()
        .build()?;
    engine.discover()?;
    engine.enable_defaults()?;
    let result =
        engine.call::<generated::Main>(&PackageId::new("example.hello")?, &generated::MainArgs)?;
    assert_eq!(result.value, 12);
    engine.shutdown()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run()
}

#[cfg(test)]
mod tests {
    #[test]
    fn high_level_embed_completes() {
        super::run().expect("hello-runtime high-level lifecycle");
    }
}
