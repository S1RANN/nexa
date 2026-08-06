fn main() {
    nexa_contract::build::generate("combat_api.nidl").expect("generate Combat bindings");
}
