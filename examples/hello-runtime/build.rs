fn main() {
    nexa_contract::build::generate("hello_api.nidl").expect("generate hello bindings");
}
