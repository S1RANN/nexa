use crate::game::Cell;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnakeCommand {
    RegisterWidget(String),
    SetWidgetText { local_id: String, text: String },
    RegisterSkin(String),
    RegisterFood(FoodDefinition),
    RegisterSpawnPolicy(String),
    AddScore(i32),
    ResizeSnake(i32),
    SetSpeed(i32),
    ProposeFoodSpawn(Cell),
    ShowToast(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoodDefinition {
    pub local_id: String,
    pub length_delta: i32,
    pub score_delta: i32,
    pub speed_delta: i32,
}
