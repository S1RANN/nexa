pub mod commands;
pub mod events;
pub mod extensions;
pub mod game;
pub mod headless;
pub mod persistence;
pub mod registries;
pub mod render;
pub mod settings;

#[allow(dead_code)]
#[allow(clippy::needless_lifetimes)]
#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/snake_api.rs"));
}

pub use events::{GameEvent, GameEventKind, GameSnapshot};
pub use extensions::{ExtensionAction, ExtensionView, SnakeExtensions};
pub use game::{Cell, Direction, GameInput, SnakeGame};
