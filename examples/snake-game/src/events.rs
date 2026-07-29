use crate::game::Cell;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameSnapshot {
    pub score: i32,
    pub total_plays: i64,
    pub foods: i32,
    pub snake_length: i32,
    pub width: i32,
    pub height: i32,
    pub food_kind: String,
    pub food: Option<Cell>,
    pub available: Vec<Cell>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameEventKind {
    PackageEnabled,
    GameStarted,
    GameTick,
    ScoreChanged,
    FoodSpawnRequested,
    FoodEaten,
    GameEnded,
    SettingsChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameEvent {
    pub kind: GameEventKind,
    pub snapshot: GameSnapshot,
}

impl GameEvent {
    #[must_use]
    pub const fn new(kind: GameEventKind, snapshot: GameSnapshot) -> Self {
        Self { kind, snapshot }
    }
}
