use macroquad::prelude::*;

use crate::extensions::SnakeExtensions;
use crate::game::{BOARD_HEIGHT, BOARD_WIDTH, SnakeGame};

const CELL_SIZE: f32 = 28.0;
const OFFSET_X: f32 = 24.0;
const OFFSET_Y: f32 = 72.0;
const PANEL: Color = Color::new(0.07, 0.10, 0.16, 0.96);
const PANEL_BORDER: Color = Color::new(0.25, 0.42, 0.56, 1.0);
const SELECTED: Color = Color::new(0.10, 0.35, 0.42, 1.0);

#[allow(clippy::cast_precision_loss)]
pub fn draw_game(game: &SnakeGame, extensions: &SnakeExtensions) {
    draw_game_scene(game, extensions);
    draw_text(
        "Move: arrows/WASD  ·  Space: restart  ·  Esc: pause",
        OFFSET_X,
        screen_height() - 10.0,
        17.0,
        GRAY,
    );
}

pub fn draw_main_menu(game: &SnakeGame, extensions: &SnakeExtensions, selected_item: usize) {
    clear_background(Color::from_rgba(10, 14, 22, 255));
    let center = screen_width() * 0.5;
    draw_centered("NEXA SNAKE", center, 150.0, 54, SKYBLUE);
    draw_centered(
        &format!(
            "Last score {}  ·  Total plays {}  ·  {}",
            game.score(),
            game.total_plays(),
            if extensions.view().safe_mode {
                "Safe Mode"
            } else {
                "Extensions Ready"
            }
        ),
        center,
        190.0,
        19,
        LIGHTGRAY,
    );
    draw_menu(
        center,
        260.0,
        &["Start / Continue", "Settings", "Quit"],
        selected_item,
    );
    draw_centered(
        "Up/Down or W/S: select  ·  Enter/Space: confirm  ·  Esc: quit",
        center,
        screen_height() - 32.0,
        17,
        GRAY,
    );
}

#[allow(clippy::cast_precision_loss)]
pub fn draw_pause_menu(game: &SnakeGame, extensions: &SnakeExtensions, selected_item: usize) {
    draw_game_scene(game, extensions);
    draw_rectangle(
        0.0,
        0.0,
        screen_width(),
        screen_height(),
        Color::new(0.0, 0.0, 0.0, 0.66),
    );
    let width = 380.0;
    let height = 330.0;
    let x = (screen_width() - width) * 0.5;
    let y = (screen_height() - height) * 0.5;
    draw_panel(x, y, width, height);
    draw_centered("PAUSED", screen_width() * 0.5, y + 64.0, 36, WHITE);
    draw_menu(
        screen_width() * 0.5,
        y + 118.0,
        &["Continue", "Settings", "Back to Main Menu"],
        selected_item,
    );
    draw_centered(
        "Esc: continue",
        screen_width() * 0.5,
        y + height - 22.0,
        16,
        GRAY,
    );
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
pub fn draw_settings(extensions: &SnakeExtensions, selected_item: usize, opened_from_pause: bool) {
    clear_background(Color::from_rgba(10, 14, 22, 255));
    let view = extensions.view();
    draw_text("SETTINGS", 32.0, 52.0, 38.0, SKYBLUE);
    draw_text(
        if opened_from_pause {
            "Game paused"
        } else {
            "Main menu"
        },
        32.0,
        78.0,
        17.0,
        GRAY,
    );

    let list_x = 32.0;
    let list_y = 108.0;
    let list_width = (screen_width() * 0.52).max(420.0);
    let row_height = 34.0;
    draw_panel(
        list_x - 12.0,
        list_y - 26.0,
        list_width,
        screen_height() - list_y - 18.0,
    );
    draw_setting_row(
        list_x,
        list_y,
        list_width - 24.0,
        "Snake Skin",
        view.selected_skin.as_deref().unwrap_or("Rust fallback"),
        selected_item == 0,
    );
    draw_setting_row(
        list_x,
        list_y + row_height,
        list_width - 24.0,
        "Food Spawn",
        view.selected_spawn_policy
            .as_deref()
            .unwrap_or("Rust safe spawn"),
        selected_item == 1,
    );
    draw_text(
        "EXTENSION PACKAGES",
        list_x,
        list_y + row_height * 2.0 + 14.0,
        16.0,
        SKYBLUE,
    );
    for (index, package) in view.packages.iter().enumerate() {
        let y = list_y + row_height * (index as f32 + 3.45);
        let status = if package.activation == nexa_embed::ActivationPolicy::Required {
            "Required · Cannot disable".to_owned()
        } else {
            format!("{:?}", package.status)
        };
        draw_setting_row(
            list_x,
            y,
            list_width - 24.0,
            &package.name,
            &status,
            selected_item == index + 2,
        );
    }

    let detail_x = list_x + list_width + 22.0;
    let detail_width = screen_width() - detail_x - 26.0;
    draw_panel(
        detail_x,
        list_y - 26.0,
        detail_width,
        screen_height() - list_y - 18.0,
    );
    draw_text("DETAILS", detail_x + 18.0, list_y + 4.0, 19.0, SKYBLUE);
    let mut y = list_y + 38.0;
    if selected_item < 2 {
        for line in [
            "Use Left/Right to change.",
            "Changes apply immediately.",
            "",
            "Package controls:",
            "Enter: enable / disable",
            "R: Restart Reload",
            "L: grant / revoke DLC access",
        ] {
            draw_text(line, detail_x + 18.0, y, 16.0, LIGHTGRAY);
            y += 25.0;
        }
    } else if let Some(package) = view.packages.get(selected_item - 2) {
        let source = match package.source_id.as_str() {
            "snake-builtin" => "Built-in",
            "snake-dlc" => "Official DLC",
            "snake-mod" => "Trusted Local Mod",
            _ => "Package",
        };
        for line in [
            package.name.clone(),
            package.id.to_string(),
            format!("Source: {source}"),
            format!("Version: {}", package.version),
            if package.activation == nexa_embed::ActivationPolicy::Required {
                "Activation: Required (cannot disable)".to_owned()
            } else {
                format!("Activation: {:?}", package.activation)
            },
            format!("Status: {:?}", package.status),
            format!(
                "Capabilities: {}",
                package
                    .capabilities
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!(
                "Recent error: {}",
                package
                    .last_diagnostic
                    .as_ref()
                    .map_or("none", |diagnostic| diagnostic.message.as_str())
            ),
        ] {
            draw_wrapped_text(
                &line,
                detail_x + 18.0,
                &mut y,
                detail_width - 36.0,
                16,
                LIGHTGRAY,
            );
        }
    }
    if let Some(toast) = &view.toast {
        draw_wrapped_text(
            toast,
            detail_x + 18.0,
            &mut y,
            detail_width - 36.0,
            16,
            YELLOW,
        );
    }
    draw_text(
        "Up/Down or W/S: select  ·  Left/Right: change  ·  Enter: action  ·  Esc: back",
        32.0,
        screen_height() - 8.0,
        16.0,
        GRAY,
    );
}

#[allow(clippy::cast_precision_loss)]
fn draw_game_scene(game: &SnakeGame, extensions: &SnakeExtensions) {
    clear_background(Color::from_rgba(10, 14, 22, 255));
    let view = extensions.view();
    draw_text(
        format!("Nexa Snake  ·  Score {}", game.score()),
        OFFSET_X,
        38.0,
        30.0,
        WHITE,
    );
    draw_rectangle_lines(
        OFFSET_X,
        OFFSET_Y,
        BOARD_WIDTH as f32 * CELL_SIZE,
        BOARD_HEIGHT as f32 * CELL_SIZE,
        2.0,
        GRAY,
    );
    let snake_color = if view
        .selected_skin
        .as_deref()
        .is_some_and(|skin| skin.contains("neon"))
    {
        Color::from_rgba(0, 255, 190, 255)
    } else {
        Color::from_rgba(80, 210, 110, 255)
    };
    for cell in game.snake() {
        draw_rectangle(
            OFFSET_X + cell.x as f32 * CELL_SIZE + 2.0,
            OFFSET_Y + cell.y as f32 * CELL_SIZE + 2.0,
            CELL_SIZE - 4.0,
            CELL_SIZE - 4.0,
            snake_color,
        );
    }
    if let Some(food) = game.food() {
        draw_circle(
            OFFSET_X + (food.x as f32 + 0.5) * CELL_SIZE,
            OFFSET_Y + (food.y as f32 + 0.5) * CELL_SIZE,
            CELL_SIZE * 0.35,
            ORANGE,
        );
    }
    let panel_x = OFFSET_X + BOARD_WIDTH as f32 * CELL_SIZE + 24.0;
    draw_text("Extension HUD", panel_x, OFFSET_Y + 24.0, 22.0, SKYBLUE);
    let mut y = OFFSET_Y + 54.0;
    for (_, text) in &view.widgets {
        draw_text(text, panel_x, y, 18.0, WHITE);
        y += 25.0;
    }
    if view.safe_mode {
        draw_text("SAFE MODE", panel_x, y + 18.0, 22.0, ORANGE);
    }
    if !game.running() {
        draw_centered(
            "GAME OVER  ·  SPACE TO RESTART",
            OFFSET_X + BOARD_WIDTH as f32 * CELL_SIZE * 0.5,
            OFFSET_Y + BOARD_HEIGHT as f32 * CELL_SIZE * 0.5,
            26,
            ORANGE,
        );
    }
    if let Some(toast) = &view.toast {
        draw_text(toast, OFFSET_X, screen_height() - 34.0, 17.0, YELLOW);
    }
}

fn draw_menu(center_x: f32, start_y: f32, items: &[&str], selected: usize) {
    for (index, item) in items.iter().enumerate() {
        let row = f32::from(u16::try_from(index).unwrap_or(u16::MAX));
        let y = start_y + row * 58.0;
        if index == selected {
            draw_rectangle(center_x - 160.0, y - 34.0, 320.0, 46.0, SELECTED);
            draw_rectangle_lines(center_x - 160.0, y - 34.0, 320.0, 46.0, 2.0, SKYBLUE);
        }
        draw_centered(item, center_x, y, 25, WHITE);
    }
}

fn draw_setting_row(x: f32, y: f32, width: f32, label: &str, value: &str, selected: bool) {
    if selected {
        draw_rectangle(x - 6.0, y - 23.0, width, 30.0, SELECTED);
    }
    draw_text(label, x, y, 17.0, WHITE);
    let dimensions = measure_text(value, None, 16, 1.0);
    draw_text(
        value,
        x + width - dimensions.width - 12.0,
        y,
        16.0,
        if selected { SKYBLUE } else { LIGHTGRAY },
    );
}

fn draw_panel(x: f32, y: f32, width: f32, height: f32) {
    draw_rectangle(x, y, width, height, PANEL);
    draw_rectangle_lines(x, y, width, height, 2.0, PANEL_BORDER);
}

fn draw_centered(text: &str, center_x: f32, y: f32, font_size: u16, color: Color) {
    let dimensions = measure_text(text, None, font_size, 1.0);
    draw_text(
        text,
        center_x - dimensions.width * 0.5,
        y,
        f32::from(font_size),
        color,
    );
}

fn draw_wrapped_text(
    text: &str,
    x: f32,
    y: &mut f32,
    max_width: f32,
    font_size: u16,
    color: Color,
) {
    let font_size_f32 = f32::from(font_size);
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if !line.is_empty() && measure_text(&candidate, None, font_size, 1.0).width > max_width {
            draw_text(&line, x, *y, font_size_f32, color);
            *y += font_size_f32 + 7.0;
            word.clone_into(&mut line);
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        draw_text(&line, x, *y, font_size_f32, color);
    }
    *y += font_size_f32 + 10.0;
}
