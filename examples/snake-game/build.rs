fn main() {
    nexa_contract::build::generate("snake_api.nidl").expect("generate Snake bindings");
}
