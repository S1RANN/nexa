// A compact, maintained control implementation. It intentionally uses one typed method and one
// dispatch arm per API; generated bindings are compared against this realistic maintenance shape.
pub trait CombatHost {
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

pub enum HostResult {
    Value,
    Pending,
    Trap,
}

pub fn dispatch(host: &mut dyn CombatHost, id: u32, entity: i32) -> Option<HostResult> {
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

pub const STABLE_NAMES: [&str; 20] = [
    "CombatHost::apply_damage",
    "CombatHost::heal",
    "CombatHost::entity_name",
    "CombatHost::enemy_view",
    "CombatHost::combat_event",
    "CombatHost::maybe_target",
    "CombatHost::nearby",
    "CombatHost::path",
    "CombatHost::set_position",
    "CombatHost::set_targets",
    "CombatHost::upload_path",
    "CombatHost::action_lock",
    "CombatHost::world_snapshot",
    "CombatHost::play_animation",
    "CombatHost::query_path",
    "CombatHost::score",
    "CombatHost::set_enabled",
    "CombatHost::ratio",
    "CombatHost::clear_target",
    "CombatHost::inspect_events",
];
