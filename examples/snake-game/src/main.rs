use macroquad::prelude::*;
use nexa_embed::{EntitlementId, PackageStatus};
use snake_game::{Direction, GameInput, SnakeExtensions, SnakeGame, render};

const MAIN_MENU_ITEMS: usize = 3;
const PAUSE_MENU_ITEMS: usize = 3;
const SETTINGS_FIXED_ITEMS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppScreen {
    MainMenu,
    Playing,
    PauseMenu,
    Settings(SettingsReturn),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsReturn {
    MainMenu,
    PauseMenu,
}

struct UiState {
    screen: AppScreen,
    main_selection: usize,
    pause_selection: usize,
    settings_selection: usize,
    should_exit: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            screen: AppScreen::MainMenu,
            main_selection: 0,
            pause_selection: 0,
            settings_selection: 0,
            should_exit: false,
        }
    }
}

fn window_conf() -> Conf {
    Conf {
        window_title: "Nexa Snake".into(),
        window_width: 1_120,
        window_height: 680,
        high_dpi: true,
        ..Default::default()
    }
}

#[macroquad::main(window_conf)]
async fn main() {
    let mut game = SnakeGame::load().expect("load Snake settings");
    let mut extensions = SnakeExtensions::load(&game).expect("load Snake extensions");
    let mut ui = UiState::default();
    let mut elapsed = 0.0_f32;
    loop {
        handle_ui_input(&mut ui, &mut extensions);
        extensions
            .apply_pending_actions(&mut game)
            .expect("apply extension actions");
        if ui.screen == AppScreen::Playing {
            elapsed += get_frame_time();
            let speed_millis = f32::from(u16::try_from(game.speed_millis()).unwrap_or(u16::MAX));
            if elapsed * 1_000.0 >= speed_millis {
                elapsed = 0.0;
                let events = game.step(read_game_input());
                extensions
                    .handle_events(events, &mut game)
                    .expect("dispatch game events");
            }
        } else {
            elapsed = 0.0;
        }
        extensions.tick(&mut game).expect("tick extensions");
        draw_screen(&ui, &game, &extensions);
        if ui.should_exit {
            break;
        }
        next_frame().await;
    }
    extensions
        .shutdown(game.total_plays())
        .expect("shutdown extensions");
}

fn handle_ui_input(ui: &mut UiState, extensions: &mut SnakeExtensions) {
    match ui.screen {
        AppScreen::MainMenu => handle_main_menu(ui),
        AppScreen::Playing => {
            if is_key_pressed(KeyCode::Escape) {
                ui.pause_selection = 0;
                ui.screen = AppScreen::PauseMenu;
            }
        }
        AppScreen::PauseMenu => handle_pause_menu(ui),
        AppScreen::Settings(return_to) => handle_settings(ui, extensions, return_to),
    }
}

fn handle_main_menu(ui: &mut UiState) {
    navigate(&mut ui.main_selection, MAIN_MENU_ITEMS);
    if is_key_pressed(KeyCode::Escape) {
        ui.should_exit = true;
    }
    if !confirm_pressed() {
        return;
    }
    match ui.main_selection {
        0 => ui.screen = AppScreen::Playing,
        1 => {
            ui.settings_selection = 0;
            ui.screen = AppScreen::Settings(SettingsReturn::MainMenu);
        }
        2 => ui.should_exit = true,
        _ => {}
    }
}

fn handle_pause_menu(ui: &mut UiState) {
    navigate(&mut ui.pause_selection, PAUSE_MENU_ITEMS);
    if is_key_pressed(KeyCode::Escape) {
        ui.screen = AppScreen::Playing;
        return;
    }
    if !confirm_pressed() {
        return;
    }
    match ui.pause_selection {
        0 => ui.screen = AppScreen::Playing,
        1 => {
            ui.settings_selection = 0;
            ui.screen = AppScreen::Settings(SettingsReturn::PauseMenu);
        }
        2 => {
            ui.main_selection = 0;
            ui.screen = AppScreen::MainMenu;
        }
        _ => {}
    }
}

fn handle_settings(ui: &mut UiState, extensions: &mut SnakeExtensions, return_to: SettingsReturn) {
    let packages = extensions.packages();
    let item_count = SETTINGS_FIXED_ITEMS + packages.len();
    navigate(&mut ui.settings_selection, item_count);
    if is_key_pressed(KeyCode::Escape) {
        ui.screen = match return_to {
            SettingsReturn::MainMenu => AppScreen::MainMenu,
            SettingsReturn::PauseMenu => AppScreen::PauseMenu,
        };
        return;
    }

    let previous = is_key_pressed(KeyCode::Left);
    let next = is_key_pressed(KeyCode::Right) || confirm_pressed();
    match ui.settings_selection {
        0 if previous || next => cycle_skin(extensions, previous),
        1 if previous || next => cycle_spawn_policy(extensions, previous),
        selected if selected >= SETTINGS_FIXED_ITEMS => {
            let Some(package) = packages.get(selected - SETTINGS_FIXED_ITEMS) else {
                return;
            };
            if confirm_pressed() {
                toggle_package(extensions, package);
            }
            if is_key_pressed(KeyCode::R) && package.status == PackageStatus::Enabled {
                extensions.queue_reload(package.id.clone());
            }
            if is_key_pressed(KeyCode::L) && package.id.as_str() == "official.food-chaos" {
                toggle_food_chaos_entitlement(extensions, package.status);
            }
        }
        _ => {}
    }
}

fn toggle_package(extensions: &mut SnakeExtensions, package: &nexa_embed::PackageInfo) {
    if package.activation == nexa_embed::ActivationPolicy::Required {
        extensions.show_toast(format!(
            "{} is required and cannot be disabled",
            package.name
        ));
        return;
    }
    match package.status {
        PackageStatus::Enabled => extensions.queue_disable(package.id.clone()),
        PackageStatus::Locked if package.id.as_str() == "official.food-chaos" => {
            toggle_food_chaos_entitlement(extensions, package.status);
            extensions.queue_enable(package.id.clone());
        }
        PackageStatus::Disabled | PackageStatus::Faulted => {
            extensions.queue_enable(package.id.clone());
        }
        _ => {}
    }
}

fn toggle_food_chaos_entitlement(extensions: &mut SnakeExtensions, status: PackageStatus) {
    let entitlements = if status == PackageStatus::Locked {
        vec![EntitlementId::new("official.food-chaos").expect("static entitlement ID is valid")]
    } else {
        Vec::new()
    };
    extensions
        .set_entitlements(entitlements)
        .expect("update DLC entitlement");
}

fn cycle_skin(extensions: &mut SnakeExtensions, backwards: bool) {
    let view = extensions.view();
    if view.skins.is_empty() {
        return;
    }
    let current = view
        .selected_skin
        .as_ref()
        .and_then(|selected| view.skins.iter().position(|value| value == selected))
        .unwrap_or_default();
    let next = wrapped_index(current, view.skins.len(), backwards);
    let _ = extensions.select_skin(&view.skins[next]);
}

fn cycle_spawn_policy(extensions: &mut SnakeExtensions, backwards: bool) {
    let view = extensions.view();
    if view.spawn_policies.is_empty() {
        return;
    }
    let current = view
        .selected_spawn_policy
        .as_ref()
        .and_then(|selected| {
            view.spawn_policies
                .iter()
                .position(|value| value == selected)
        })
        .unwrap_or_default();
    let next = wrapped_index(current, view.spawn_policies.len(), backwards);
    let _ = extensions.select_spawn_policy(&view.spawn_policies[next]);
}

fn navigate(selected: &mut usize, item_count: usize) {
    if item_count == 0 {
        *selected = 0;
        return;
    }
    *selected = (*selected).min(item_count - 1);
    if is_key_pressed(KeyCode::Up) || is_key_pressed(KeyCode::W) {
        *selected = wrapped_index(*selected, item_count, true);
    }
    if is_key_pressed(KeyCode::Down) || is_key_pressed(KeyCode::S) {
        *selected = wrapped_index(*selected, item_count, false);
    }
}

fn wrapped_index(current: usize, len: usize, backwards: bool) -> usize {
    if backwards {
        current.checked_sub(1).unwrap_or(len - 1)
    } else {
        (current + 1) % len
    }
}

fn confirm_pressed() -> bool {
    is_key_pressed(KeyCode::Enter) || is_key_pressed(KeyCode::Space)
}

fn read_game_input() -> GameInput {
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

fn draw_screen(ui: &UiState, game: &SnakeGame, extensions: &SnakeExtensions) {
    match ui.screen {
        AppScreen::MainMenu => render::draw_main_menu(game, extensions, ui.main_selection),
        AppScreen::Playing => render::draw_game(game, extensions),
        AppScreen::PauseMenu => {
            render::draw_pause_menu(game, extensions, ui.pause_selection);
        }
        AppScreen::Settings(return_to) => {
            render::draw_settings(
                extensions,
                ui.settings_selection,
                matches!(return_to, SettingsReturn::PauseMenu),
            );
        }
    }
}
