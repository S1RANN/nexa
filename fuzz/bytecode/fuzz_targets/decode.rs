#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let _ = nexa_bytecode::Module::decode_with_limits(
        bytes,
        nexa_bytecode::DecodeLimits {
            max_bytes: 1 << 20,
            max_sections: 16,
            max_functions: 1_024,
            max_instructions: 16_384,
            max_registers: 1_024,
            max_root_maps: 4_096,
            max_loop_bounds: 4_096,
            max_host_imports: 1_024,
            max_state_types: 1_024,
            max_enum_types: 1_024,
            max_exports: 1_024,
            max_source_map_entries: 4_096,
            ..nexa_bytecode::DecodeLimits::default()
        },
    );
});
