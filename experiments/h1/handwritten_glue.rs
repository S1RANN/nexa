pub enum HostValue {
    I32(i32),
}

pub trait CombatHost {
    fn apply_damage(&mut self, entity: i32, amount: i32) -> i32;
    fn heal(&mut self, entity: i32, amount: i32) -> i32;
    fn add_armor(&mut self, entity: i32, amount: i32) -> i32;
    fn remove_armor(&mut self, entity: i32, amount: i32) -> i32;
    fn add_mana(&mut self, entity: i32, amount: i32) -> i32;
    fn spend_mana(&mut self, entity: i32, amount: i32) -> i32;
    fn add_stamina(&mut self, entity: i32, amount: i32) -> i32;
    fn spend_stamina(&mut self, entity: i32, amount: i32) -> i32;
    fn add_threat(&mut self, entity: i32, amount: i32) -> i32;
    fn clear_threat(&mut self, entity: i32, amount: i32) -> i32;
    fn start_cooldown(&mut self, entity: i32, amount: i32) -> i32;
    fn reduce_cooldown(&mut self, entity: i32, amount: i32) -> i32;
    fn add_combo(&mut self, entity: i32, amount: i32) -> i32;
    fn consume_combo(&mut self, entity: i32, amount: i32) -> i32;
    fn set_target(&mut self, entity: i32, amount: i32) -> i32;
    fn clear_target(&mut self, entity: i32, amount: i32) -> i32;
    fn set_phase(&mut self, entity: i32, amount: i32) -> i32;
    fn advance_phase(&mut self, entity: i32, amount: i32) -> i32;
    fn add_score(&mut self, entity: i32, amount: i32) -> i32;
    fn update_rank(&mut self, entity: i32, amount: i32) -> i32;
}

pub fn dispatch(
    host: &mut dyn CombatHost,
    id: u32,
    entity: i32,
    amount: i32,
) -> Option<HostValue> {
    let value = match id {
        0 => host.apply_damage(entity, amount),
        1 => host.heal(entity, amount),
        2 => host.add_armor(entity, amount),
        3 => host.remove_armor(entity, amount),
        4 => host.add_mana(entity, amount),
        5 => host.spend_mana(entity, amount),
        6 => host.add_stamina(entity, amount),
        7 => host.spend_stamina(entity, amount),
        8 => host.add_threat(entity, amount),
        9 => host.clear_threat(entity, amount),
        10 => host.start_cooldown(entity, amount),
        11 => host.reduce_cooldown(entity, amount),
        12 => host.add_combo(entity, amount),
        13 => host.consume_combo(entity, amount),
        14 => host.set_target(entity, amount),
        15 => host.clear_target(entity, amount),
        16 => host.set_phase(entity, amount),
        17 => host.advance_phase(entity, amount),
        18 => host.add_score(entity, amount),
        19 => host.update_rank(entity, amount),
        _ => return None,
    };
    Some(HostValue::I32(value))
}

pub const STABLE_NAMES: [&str; 20] = [
    "CombatHost::apply_damage",
    "CombatHost::heal",
    "CombatHost::add_armor",
    "CombatHost::remove_armor",
    "CombatHost::add_mana",
    "CombatHost::spend_mana",
    "CombatHost::add_stamina",
    "CombatHost::spend_stamina",
    "CombatHost::add_threat",
    "CombatHost::clear_threat",
    "CombatHost::start_cooldown",
    "CombatHost::reduce_cooldown",
    "CombatHost::add_combo",
    "CombatHost::consume_combo",
    "CombatHost::set_target",
    "CombatHost::clear_target",
    "CombatHost::set_phase",
    "CombatHost::advance_phase",
    "CombatHost::add_score",
    "CombatHost::update_rank",
];
