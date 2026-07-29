fn main() {
    let source = std::fs::read_to_string("hello_api.nidl").expect("hello_api.nidl");
    let idl = nexa_idl::parse(&source).expect("valid hello API NIDL");
    let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
        .join("hello_api.rs");
    std::fs::write(output, nexa_idl::generate_rust(&idl)).expect("write generated bindings");
    println!("cargo:rerun-if-changed=hello_api.nidl");
}
