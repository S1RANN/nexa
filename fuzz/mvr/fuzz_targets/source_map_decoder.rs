#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_INPUT: usize = 1 << 20;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > MAX_INPUT {
        return;
    }
    let seed_module = (bytes == b"NXBC source-map\n").then(|| {
        nexa_compiler::compile("fn identity(value: i32) -> i32 { return value; }")
            .expect("fixed source compiles")
            .module()
            .encode()
    });
    let bytes = seed_module.as_deref().unwrap_or(bytes);
    let _ = nexa_bytecode::Module::decode_with_limits(
        bytes,
        nexa_bytecode::DecodeLimits {
            max_bytes: MAX_INPUT,
            max_sections: 32,
            max_functions: 256,
            max_instructions: 8_192,
            max_registers: 512,
            max_root_maps: 2_048,
            max_loop_bounds: 2_048,
            max_host_imports: 256,
            max_state_types: 256,
            max_enum_types: 256,
            max_exports: 256,
            max_source_map_entries: 2_048,
        },
    );
});
