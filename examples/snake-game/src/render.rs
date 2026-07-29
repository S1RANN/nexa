use macroquad::prelude::*;

use crate::extensions::SnakeExtensions;
use crate::game::{BOARD_HEIGHT, BOARD_WIDTH, SnakeGame};

const CELL_SIZE: f32 = 28.0;
const OFFSET_X: f32 = 24.0;
const OFFSET_Y: f32 = 72.0;

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
pub fn draw(game: &SnakeGame, extensions: &SnakeExtensions, selected_package: usize) {
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
    draw_text("Extension HUD", panel_x, OFFSET_Y + 24.0, 24.0, SKYBLUE);
    let mut y = OFFSET_Y + 54.0;
    for (_, text) in &view.widgets {
        draw_text(text, panel_x, y, 20.0, WHITE);
        y += 26.0;
    }
    draw_text("Packages", panel_x, y + 18.0, 24.0, SKYBLUE);
    y += 48.0;
    for (index, package) in view.packages.iter().take(9).enumerate() {
        draw_text(
            format!(
                "{} {} {}",
                if index == selected_package {
                    "▶"
                } else {
                    " "
                },
                package.name,
                package.id
            ),
            panel_x,
            y,
            16.0,
            if package.status == nexa_embed::PackageStatus::Enabled {
                GREEN
            } else {
                LIGHTGRAY
            },
        );
        y += 20.0;
    }
    if let Some(package) = view.packages.get(selected_package) {
        y += 8.0;
        let source = match package.source_id.as_str() {
            "snake-builtin" => "Built-in",
            "snake-dlc" => "Official DLC",
            "snake-mod" => "Trusted Local Mod",
            _ => "Package",
        };
        for line in [
            format!("Source: {source} ({})", package.source_id),
            format!("Version: {}", package.version.as_str()),
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
                package.last_error.as_deref().unwrap_or("none")
            ),
        ] {
            draw_text(&line, panel_x, y, 15.0, LIGHTGRAY);
            y += 18.0;
        }
    }
    y += 8.0;
    draw_text(
        format!(
            "Skin: {}",
            view.selected_skin.as_deref().unwrap_or("Rust fallback")
        ),
        panel_x,
        y,
        16.0,
        WHITE,
    );
    y += 20.0;
    draw_text(
        format!(
            "Spawn: {}",
            view.selected_spawn_policy
                .as_deref()
                .unwrap_or("Rust safe spawn")
        ),
        panel_x,
        y,
        16.0,
        WHITE,
    );
    if let Some(toast) = &view.toast {
        draw_text(toast, OFFSET_X, screen_height() - 24.0, 18.0, YELLOW);
    }
    if view.safe_mode {
        draw_text("SAFE MODE", panel_x, screen_height() - 48.0, 24.0, ORANGE);
    }
    draw_text(
        "Move: arrows/WASD · Packages: PgUp/PgDn + E/X/R/L · K skin · P spawn",
        OFFSET_X,
        screen_height() - 4.0,
        16.0,
        GRAY,
    );
}
