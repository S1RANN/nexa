fn main() {
    nexa_idl::build::generate("host_matrix.nidl")
        .expect("validate and generate host matrix bindings");
}
