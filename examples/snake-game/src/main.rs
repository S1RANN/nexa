use macroquad::prelude::*;
use nexa_embed::{EntitlementId, PackageStatus};
use snake_game::{Direction, GameInput, SnakeExtensions, SnakeGame, render};

#[macroquad::main("Nexa Snake")]
async fn main() {
    let mut game = SnakeGame::load().expect("load Snake settings");
    let mut extensions = SnakeExtensions::load(&game).expect("load Snake extensions");
    let mut elapsed = 0.0_f32;
    let mut selected_package = 0_usize;
    loop {
        if is_key_pressed(KeyCode::Escape) {
            break;
        }
        handle_settings_input(&mut extensions, &mut selected_package);
        extensions
            .apply_pending_actions(&mut game)
            .expect("apply extension actions");
        elapsed += get_frame_time();
        let speed_millis = f32::from(u16::try_from(game.speed_millis()).unwrap_or(u16::MAX));
        if elapsed * 1_000.0 >= speed_millis {
            elapsed = 0.0;
            let events = game.step(read_input());
            extensions
                .handle_events(events, &mut game)
                .expect("dispatch game events");
        }
        extensions.tick(&mut game).expect("tick extensions");
        render::draw(&game, &extensions, selected_package);
        next_frame().await;
    }
    extensions
        .shutdown(game.total_plays())
        .expect("shutdown extensions");
}

fn read_input() -> GameInput {
    let direction = if is_key_down(KeyCode::Up) || is_key_down(KeyCode::W) {
        Some(Direction::Up)
    } else if is_key_down(KeyCode::Down) || is_key_down(KeyCode::S) {
        Some(Direction::Down)
    } else if is_key_down(KeyCode::Left) || is_key_down(KeyCode::A) {
        Some(Direction::Left)
    } else if is_key_down(KeyCode::Right) || is_key_down(KeyCode::D) {
        Some(Direction::Right)
    } else {
        None
    };
    GameInput {
        direction,
        restart: is_key_pressed(KeyCode::Space),
    }
}

fn handle_settings_input(extensions: &mut SnakeExtensions, selected: &mut usize) {
    let packages = extensions.packages();
    if packages.is_empty() {
        return;
    }
    if is_key_pressed(KeyCode::PageUp) {
        *selected = selected.saturating_sub(1);
    }
    if is_key_pressed(KeyCode::PageDown) {
        *selected = (*selected + 1).min(packages.len() - 1);
    }
    if let Some(package) = packages.get(*selected) {
        if is_key_pressed(KeyCode::E) {
            extensions.queue_enable(package.id.clone());
        }
        if is_key_pressed(KeyCode::X) {
            extensions.queue_disable(package.id.clone());
        }
        if is_key_pressed(KeyCode::R) {
            extensions.queue_reload(package.id.clone());
        }
        if package.id.as_str() == "official.food-chaos" && is_key_pressed(KeyCode::L) {
            let entitlements = if package.status == PackageStatus::Locked {
                vec![
                    EntitlementId::new("official.food-chaos")
                        .expect("static entitlement ID is valid"),
                ]
            } else {
                Vec::new()
            };
            extensions
                .set_entitlements(entitlements)
                .expect("update DLC entitlement");
        }
    }
    let view = extensions.view();
    if is_key_pressed(KeyCode::K) && !view.skins.is_empty() {
        let current = view
            .selected_skin
            .as_ref()
            .and_then(|current| view.skins.iter().position(|value| value == current))
            .unwrap_or_default();
        let next = &view.skins[(current + 1) % view.skins.len()];
        let _ = extensions.select_skin(next);
    }
    if is_key_pressed(KeyCode::P) && !view.spawn_policies.is_empty() {
        let current = view
            .selected_spawn_policy
            .as_ref()
            .and_then(|current| {
                view.spawn_policies
                    .iter()
                    .position(|value| value == current)
            })
            .unwrap_or_default();
        let next = &view.spawn_policies[(current + 1) % view.spawn_policies.len()];
        let _ = extensions.select_spawn_policy(next);
    }
}
