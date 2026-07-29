use std::collections::VecDeque;

use crate::events::{GameEvent, GameEventKind, GameSnapshot};

pub const BOARD_WIDTH: i32 = 24;
pub const BOARD_HEIGHT: i32 = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Cell {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GameInput {
    pub direction: Option<Direction>,
    pub restart: bool,
}

pub struct SnakeGame {
    snake: VecDeque<Cell>,
    direction: Direction,
    food: Option<Cell>,
    food_kind: String,
    score: i32,
    total_plays: i64,
    foods: i32,
    pending_growth: i32,
    speed_millis: u64,
    speed_ticks_remaining: u32,
    spawn_requested: bool,
    running: bool,
    rng: u64,
}

impl SnakeGame {
    pub fn load() -> Result<Self, std::io::Error> {
        let settings = crate::persistence::load(
            &crate::persistence::default_data_root().join("settings.json"),
        )?;
        Ok(Self::new(settings.total_plays))
    }

    #[must_use]
    pub fn new(total_plays: i64) -> Self {
        let mut game = Self {
            snake: VecDeque::new(),
            direction: Direction::Right,
            food: None,
            food_kind: "classic".into(),
            score: 0,
            total_plays,
            foods: 0,
            pending_growth: 0,
            speed_millis: 140,
            speed_ticks_remaining: 0,
            spawn_requested: false,
            running: false,
            rng: 0x4e45_5841_534e_414b,
        };
        game.start();
        game
    }

    pub fn start(&mut self) {
        self.snake = VecDeque::from([
            Cell { x: 8, y: 9 },
            Cell { x: 7, y: 9 },
            Cell { x: 6, y: 9 },
        ]);
        self.direction = Direction::Right;
        self.food = None;
        self.score = 0;
        self.foods = 0;
        self.pending_growth = 0;
        self.speed_millis = 140;
        self.speed_ticks_remaining = 0;
        self.spawn_requested = false;
        self.running = true;
        self.total_plays = self.total_plays.saturating_add(1);
    }

    pub fn step(&mut self, input: GameInput) -> Vec<GameEvent> {
        let mut events = Vec::new();
        if input.restart || !self.running {
            self.start();
            events.push(GameEvent::new(GameEventKind::GameStarted, self.snapshot()));
        }
        if let Some(direction) = input.direction
            && !opposite(self.direction, direction)
        {
            self.direction = direction;
        }
        if self.food.is_none() {
            if self.spawn_requested {
                self.spawn_safe_food();
                self.spawn_requested = false;
            } else {
                self.spawn_requested = true;
                events.push(GameEvent::new(
                    GameEventKind::FoodSpawnRequested,
                    self.snapshot(),
                ));
                return events;
            }
        }
        let Some(head) = self.snake.front().copied() else {
            return events;
        };
        let next = match self.direction {
            Direction::Up => Cell {
                x: head.x,
                y: head.y - 1,
            },
            Direction::Down => Cell {
                x: head.x,
                y: head.y + 1,
            },
            Direction::Left => Cell {
                x: head.x - 1,
                y: head.y,
            },
            Direction::Right => Cell {
                x: head.x + 1,
                y: head.y,
            },
        };
        if !Self::valid_cell(next) || self.snake.contains(&next) {
            self.running = false;
            events.push(GameEvent::new(GameEventKind::GameEnded, self.snapshot()));
            return events;
        }
        self.snake.push_front(next);
        if self.food == Some(next) {
            self.foods = self.foods.saturating_add(1);
            self.score = self.score.saturating_add(10);
            self.pending_growth = self.pending_growth.saturating_add(1);
            let snapshot = self.snapshot();
            events.push(GameEvent::new(GameEventKind::FoodEaten, snapshot.clone()));
            events.push(GameEvent::new(GameEventKind::ScoreChanged, snapshot));
            self.food = None;
        }
        if self.pending_growth > 0 {
            self.pending_growth -= 1;
        } else {
            self.snake.pop_back();
        }
        events.push(GameEvent::new(GameEventKind::GameTick, self.snapshot()));
        if self.speed_ticks_remaining > 0 {
            self.speed_ticks_remaining -= 1;
            if self.speed_ticks_remaining == 0 {
                self.speed_millis = 140;
            }
        }
        events
    }

    pub fn resize(&mut self, delta: i32) {
        if delta > 0 {
            self.pending_growth = self.pending_growth.saturating_add(delta);
        } else {
            for _ in 0..delta.unsigned_abs() {
                if self.snake.len() > 2 {
                    self.snake.pop_back();
                }
            }
        }
    }

    pub fn add_score(&mut self, delta: i32) {
        self.score = self.score.saturating_add(delta).max(0);
    }

    pub fn set_speed_delta(&mut self, delta: i32) {
        let speed = i64::try_from(self.speed_millis)
            .unwrap_or(i64::MAX)
            .saturating_add(i64::from(delta));
        self.speed_millis = u64::try_from(speed.clamp(50, 500)).unwrap_or(140);
        self.speed_ticks_remaining = 30;
    }

    pub fn propose_food(&mut self, cell: Cell) -> bool {
        if self.food.is_none() && Self::valid_cell(cell) && !self.snake.contains(&cell) {
            self.food = Some(cell);
            self.spawn_requested = false;
            true
        } else {
            false
        }
    }

    fn spawn_safe_food(&mut self) {
        let available = self.available_cells();
        if available.is_empty() {
            self.running = false;
            return;
        }
        self.rng = self
            .rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let index = usize::try_from(self.rng).unwrap_or(0) % available.len();
        self.food = Some(available[index]);
        self.food_kind = "classic".into();
    }

    fn valid_cell(cell: Cell) -> bool {
        (0..BOARD_WIDTH).contains(&cell.x) && (0..BOARD_HEIGHT).contains(&cell.y)
    }

    #[must_use]
    pub fn available_cells(&self) -> Vec<Cell> {
        (0..BOARD_HEIGHT)
            .flat_map(|y| (0..BOARD_WIDTH).map(move |x| Cell { x, y }))
            .filter(|cell| !self.snake.contains(cell))
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> GameSnapshot {
        GameSnapshot {
            score: self.score,
            total_plays: self.total_plays,
            foods: self.foods,
            snake_length: i32::try_from(self.snake.len()).unwrap_or(i32::MAX),
            width: BOARD_WIDTH,
            height: BOARD_HEIGHT,
            food_kind: self.food_kind.clone(),
            food: self.food,
            available: self.available_cells(),
        }
    }

    #[must_use]
    pub fn snake(&self) -> &VecDeque<Cell> {
        &self.snake
    }

    #[must_use]
    pub const fn food(&self) -> Option<Cell> {
        self.food
    }

    #[must_use]
    pub const fn score(&self) -> i32 {
        self.score
    }

    #[must_use]
    pub const fn total_plays(&self) -> i64 {
        self.total_plays
    }

    #[must_use]
    pub const fn speed_millis(&self) -> u64 {
        self.speed_millis
    }

    #[must_use]
    pub const fn running(&self) -> bool {
        self.running
    }

    pub fn set_food_kind(&mut self, kind: impl Into<String>) {
        self.food_kind = kind.into();
    }
}

const fn opposite(current: Direction, candidate: Direction) -> bool {
    matches!(
        (current, candidate),
        (Direction::Up, Direction::Down)
            | (Direction::Down, Direction::Up)
            | (Direction::Left, Direction::Right)
            | (Direction::Right, Direction::Left)
    )
}
