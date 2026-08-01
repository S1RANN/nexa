use nexa_core::FileId;

fn main() {
    let source = "use std::core;\nfn value() -> i32 { return core::min_i32(2, 1); }\n";
    let verified = nexa_compiler::compile_file(source, FileId(2)).unwrap();
    let module = verified.module();
    let mut rows = module
        .source_map
        .iter()
        .map(|entry| {
            (
                entry.function,
                entry.span.file.0,
                entry.span.start,
                entry.span.end,
            )
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    rows.dedup();
    for row in rows {
        println!("{row:?}");
    }
}
