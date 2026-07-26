#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 64 {
        return;
    }
    let value = bytes.first().copied().unwrap_or_default();
    let source = format!(
        "enum Failure {{ Rejected }}
         fn propagate(value: Result<i32, Failure>) -> Result<i32, Failure> {{
             return Ok(value? + {value});
         }}"
    );
    let _ = nexa_compiler::compile(&source);
});
