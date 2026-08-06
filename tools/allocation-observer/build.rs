fn main() {
    nexa_contract::build::generate("host_matrix.contract.nexa")
        .expect("validate and generate host matrix bindings");
}
