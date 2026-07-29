fn main() {
    nexa_idl::build::generate("snake_api.nidl").expect("generate Snake bindings");
}
