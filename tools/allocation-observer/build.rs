fn main() {
    let source = std::fs::read_to_string("host_matrix.idl").expect("host matrix IDL");
    let idl = nexa_idl::parse(&source).expect("valid host matrix IDL");
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
            .join("host_matrix.rs");
    std::fs::write(output, nexa_idl::generate_rust(&idl))
        .expect("write generated host matrix bindings");
    println!("cargo:rerun-if-changed=host_matrix.idl");
}
