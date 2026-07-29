fn main() {
    nexa_idl::build::generate("hello_api.nidl").expect("generate hello bindings");
}
