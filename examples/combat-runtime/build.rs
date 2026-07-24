fn main() {
    let source = std::fs::read_to_string("engine.idl").expect("engine.idl");
    let idl = nexa_idl::parse(&source).expect("valid engine IDL");
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("engine.rs");
    std::fs::write(output, nexa_idl::generate_rust(&idl)).expect("write generated bindings");
    println!("cargo:rerun-if-changed=engine.idl");
}
