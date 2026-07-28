#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    if bytes.len() > 64 {
        return;
    }
    let some = bytes.first().copied().unwrap_or_default();
    let none = bytes.get(1).copied().unwrap_or_default();
    let source = format!(
        "fn select(value: Option<i32>) -> i32 {{
            return match value {{
                Some(found) => found + {some},
                None => {none},
            }};
        }}"
    );
    let _ = nexa_compiler::compile(&source);
});
