fn main() {
    nexa_contract::build::generate("host_matrix.nidl")
        .expect("validate and generate host matrix bindings");
}
