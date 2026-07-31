//! Regenerates the versioned binary diagnostic fixtures.
//!
//! Run from the workspace with:
//! `cargo run -p nexa-verifier --example generate_diagnostic_fixtures`

use std::fmt::Write as _;

use nexa_bytecode::{
    DecodeError, Function, FunctionBuilder, Instruction, Module, ModuleBuilder, Signature,
    SourceMapEntry,
};
use nexa_core::{FileId, SourceSpan};
use nexa_verifier::{VerifierLimits, VerifyErrorKind, verify};

pub struct EncodedFixture {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

impl EncodedFixture {
    #[must_use]
    pub fn hex(&self) -> String {
        let mut encoded = String::with_capacity(self.bytes.len() * 2);
        for byte in &self.bytes {
            write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        encoded
    }
}

#[must_use]
pub fn encoded_fixtures() -> Vec<EncodedFixture> {
    vec![
        invalid_magic(),
        register_out_of_range(),
        invalid_root_map(),
        invalid_source_map(),
        invalid_reload_metadata(),
    ]
}

fn ordinary_function(registers: u16, code: impl IntoIterator<Item = Instruction>) -> Function {
    let mut builder = FunctionBuilder::new(
        Signature {
            parameters: Vec::new(),
            result: None,
        },
        registers,
    );
    for instruction in code {
        builder.emit(instruction);
    }
    builder.finish().expect("fixture function is valid")
}

fn module_with(function: Function) -> Module {
    let mut builder = ModuleBuilder::new();
    builder.function(function);
    builder.finish()
}

fn assert_valid_base(module: &Module) {
    verify(module.clone(), VerifierLimits::default()).expect("fixture base module must verify");
}

fn invalid_magic() -> EncodedFixture {
    let module = module_with(ordinary_function(0, [Instruction::ReturnVoid]));
    assert_valid_base(&module);
    let mut bytes = module.encode();
    bytes[0] ^= u8::MAX;
    assert_eq!(Module::decode(&bytes), Err(DecodeError::InvalidMagic));
    EncodedFixture {
        name: "NX3001",
        bytes,
    }
}

fn register_out_of_range() -> EncodedFixture {
    let mut module = module_with(ordinary_function(
        1,
        [
            Instruction::LoadI32 { dst: 0, value: 7 },
            Instruction::ReturnVoid,
        ],
    ));
    assert_valid_base(&module);
    module.functions[0].code[0] = Instruction::LoadI32 { dst: 1, value: 7 };

    let bytes = module.encode();
    let decoded = Module::decode(&bytes).expect("register fixture must decode");
    let error = verify(decoded, VerifierLimits::default())
        .expect_err("register fixture must fail verification");
    assert_eq!(error.kind, VerifyErrorKind::RegisterOutOfRange(1));
    EncodedFixture {
        name: "NX3002",
        bytes,
    }
}

fn invalid_root_map() -> EncodedFixture {
    let mut module = module_with(ordinary_function(0, [Instruction::ReturnVoid]));
    assert_valid_base(&module);
    module.functions[0].root_maps[0].pc = 1;

    let bytes = module.encode();
    let decoded = Module::decode(&bytes).expect("root-map fixture must decode");
    let error = verify(decoded, VerifierLimits::default())
        .expect_err("root-map fixture must fail verification");
    assert_eq!(error.kind, VerifyErrorKind::InvalidRootMap(1));
    EncodedFixture {
        name: "NX3003",
        bytes,
    }
}

fn invalid_source_map() -> EncodedFixture {
    let function = ordinary_function(0, [Instruction::ReturnVoid]);
    let mut builder = ModuleBuilder::new();
    builder.function(function);
    builder.source_map([SourceMapEntry {
        function: 0,
        pc_start: 0,
        pc_end: 1,
        span: SourceSpan::new(FileId(1), 0, 1),
    }]);
    let mut module = builder.finish();
    assert_valid_base(&module);
    module.source_map[0].pc_end = 0;

    let bytes = module.encode();
    assert_eq!(Module::decode(&bytes), Err(DecodeError::InvalidSourceMap));
    EncodedFixture {
        name: "NX3004",
        bytes,
    }
}

fn invalid_reload_metadata() -> EncodedFixture {
    let mut module = module_with(ordinary_function(0, [Instruction::ReturnVoid]));
    assert_valid_base(&module);
    module.reload_metadata.activation_entry = Some(0);

    let bytes = module.encode();
    let decoded = Module::decode(&bytes).expect("reload-metadata fixture must decode");
    let error = verify(decoded, VerifierLimits::default())
        .expect_err("reload-metadata fixture must fail verification");
    assert_eq!(error.kind, VerifyErrorKind::InvalidReloadMetadata);
    EncodedFixture {
        name: "NX6005",
        bytes,
    }
}

#[cfg(not(test))]
fn main() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/diagnostics/binaries");
    for fixture in encoded_fixtures() {
        let path = directory.join(format!("{}.bin", fixture.name));
        std::fs::write(&path, format!("{}\n", fixture.hex()))
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", path.display()));
        println!("wrote {}", path.display());
    }
}
