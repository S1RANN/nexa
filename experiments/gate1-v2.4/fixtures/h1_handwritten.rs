#![allow(dead_code)]

pub enum HostResult {
    Value,
    Pending,
    Trap,
}

pub struct Snapshot<T>(pub T);
pub struct Buffer<T>(pub Vec<T>);
pub struct Token<T>(pub std::marker::PhantomData<T>);
pub struct CombatResource;
pub struct AlternateResource;

pub fn parameter_type(value: i32) -> i64 { i64::from(value) }
pub fn return_type(value: i32) -> i32 { value }
pub fn add_parameter(value: i32) -> i32 { value }
pub fn delete_parameter(value: i32) -> i32 { value }
pub fn parameter_order(entity: i32, amount: i64) -> i64 { i64::from(entity) + amount }
pub fn sync_to_async() -> HostResult { HostResult::Value }
pub const FUEL_COST: u32 = 1;
pub const CANCEL_POLICY: &str = "return_error";
pub const ABANDON_POLICY: &str = "return_error";

pub enum MutationError { Missing, Busy }
pub enum MutationEvent { Damage(i32), Idle }
pub struct MutationVec { pub x: i32, pub y: i32 }
pub struct MutationView { pub health: i32 }

pub fn snapshot_content() -> Snapshot<MutationView> {
    Snapshot(MutationView { health: 1 })
}
pub fn buffer_element() -> Buffer<MutationVec> {
    Buffer(vec![MutationVec { x: 1, y: 2 }])
}
pub fn resource_domain() -> Token<CombatResource> {
    Token(std::marker::PhantomData)
}

pub const STABLE_FUNCTION_ID: &str = "CombatHost::score";
pub const PRESERVED_RENAME_ID: &str = "CombatHost::ratio";
pub fn ratio(value: i32) -> i32 { value }
pub const INTERFACE_HASH: u64 = 0x1020_3040;
pub fn required_host_function(value: i32) -> i32 { value }
pub const REQUIRED_HOST_FUNCTION: fn(i32) -> i32 = required_host_function;

pub trait ManualCombatHost {
    fn apply_damage(&mut self, entity: i32, amount: i32) -> HostResult;
    fn heal(&mut self, entity: i32, amount: i32) -> HostResult;
    fn entity_name(&mut self, entity: i32) -> HostResult;
    fn enemy_view(&mut self, entity: i32) -> HostResult;
    fn combat_event(&mut self, entity: i32) -> HostResult;
    fn maybe_target(&mut self, entity: i32) -> HostResult;
    fn nearby(&mut self, entity: i32) -> HostResult;
    fn path(&mut self, entity: i32) -> HostResult;
    fn set_position(&mut self, entity: i32) -> HostResult;
    fn set_targets(&mut self, entity: i32) -> HostResult;
    fn upload_path(&mut self, entity: i32) -> HostResult;
    fn action_lock(&mut self, entity: i32) -> HostResult;
    fn world_snapshot(&mut self) -> HostResult;
    fn play_animation(&mut self, entity: i32) -> HostResult;
    fn query_path(&mut self, entity: i32) -> HostResult;
    fn score(&mut self, entity: i32) -> HostResult;
    fn set_enabled(&mut self, entity: i32) -> HostResult;
    fn ratio(&mut self, entity: i32) -> HostResult;
    fn clear_target(&mut self, entity: i32) -> HostResult;
    fn inspect_events(&mut self) -> HostResult;
}

pub fn dispatch_manual(
    host: &mut dyn ManualCombatHost,
    id: u32,
    entity: i32,
) -> Option<HostResult> {
    Some(match id {
        0 => host.apply_damage(entity, 1),
        1 => host.heal(entity, 1),
        2 => host.entity_name(entity),
        3 => host.enemy_view(entity),
        4 => host.combat_event(entity),
        5 => host.maybe_target(entity),
        6 => host.nearby(entity),
        7 => host.path(entity),
        8 => host.set_position(entity),
        9 => host.set_targets(entity),
        10 => host.upload_path(entity),
        11 => host.action_lock(entity),
        12 => host.world_snapshot(),
        13 => host.play_animation(entity),
        14 => host.query_path(entity),
        15 => host.score(entity),
        16 => host.set_enabled(entity),
        17 => host.ratio(entity),
        18 => host.clear_target(entity),
        19 => host.inspect_events(),
        _ => return None,
    })
}
