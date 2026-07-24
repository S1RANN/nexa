#[path = "runtime-baseline/explicit_yield.rs"]
mod explicit_yield;
#[path = "runtime-baseline/fast_complete.rs"]
mod fast_complete;
#[path = "runtime-baseline/fuel_yield.rs"]
mod fuel_yield;
#[path = "runtime-baseline/gc_suspended_root.rs"]
mod gc_suspended_root;
#[path = "runtime-baseline/host_request.rs"]
mod host_request;
#[path = "runtime-baseline/nested_call.rs"]
mod nested_call;
#[path = "runtime-baseline/reload_activation_fault.rs"]
mod reload_activation_fault;
#[path = "runtime-baseline/resource_token.rs"]
mod resource_token;
#[path = "runtime-baseline/scope_cancel.rs"]
mod scope_cancel;
#[path = "runtime-baseline/trap.rs"]
mod trap;

mod support;
