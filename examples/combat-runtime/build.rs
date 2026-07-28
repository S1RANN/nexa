fn main() {
    let source = std::fs::read_to_string("combat_api.nidl").expect("combat_api.nidl");
    let idl = nexa_idl::parse(&source).expect("valid combat API NIDL");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("combat_api.rs");
    std::fs::write(output, nexa_idl::generate_rust(&idl)).expect("write generated bindings");
    println!("cargo:rerun-if-changed=combat_api.nidl");
}
