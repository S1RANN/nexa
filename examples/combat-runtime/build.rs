fn main() {
    nexa_idl::build::generate("combat_api.nidl").expect("generate Combat bindings");
}
