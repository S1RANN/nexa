//! Regression coverage for the typed-compiler error surface.
//!
//! The `Display` implementation of [`nexa_compiler::CompileError`] must always
//! render a stable user-facing message. It must never fall back to the internal
//! `Debug` representation (for example `TypeMismatch { expected: None, ... }`),
//! which previously leaked through `nexa check <file>` as raw structure.

#[test]
fn compile_error_display_never_leaks_debug_structure() {
    // `fn main() { 1 }` lowers to a `TypeMismatch` in the typed compiler.
    let error = nexa_compiler::compile("fn main() { 1 }").unwrap_err();
    let rendered = error.to_string();

    assert!(
        rendered.contains("type mismatch"),
        "Display must keep the user-facing message, got: {rendered}"
    );
    assert!(
        !rendered.contains("TypeMismatch {"),
        "Display leaked the Debug variant structure: {rendered}"
    );
    assert!(
        !rendered.contains("SourceSpan {") && !rendered.contains("FileId("),
        "Display leaked internal span structure: {rendered}"
    );
}

#[test]
fn compile_error_display_reports_named_symbols() {
    let error = nexa_compiler::compile("fn main() { missing_name() }").unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.contains("missing_name"),
        "Display must name the offending symbol, got: {rendered}"
    );
}
