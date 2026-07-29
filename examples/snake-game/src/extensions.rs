use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use nexa_embed::{
    ActivationPolicy, ActivationSet, CapabilityId, CapabilitySet, DirectorySource, EntitlementId,
    EntitlementResolver, NexaEmbed, PackageId, PackageInfo, PackagePolicy, PackageRuntimeLimits,
    PackageStatus, SourceId, TrustLevel,
};

use crate::commands::{FoodDefinition, SnakeCommand};
use crate::events::{GameEvent, GameEventKind, GameSnapshot};
use crate::game::{Cell, SnakeGame};
use crate::generated;
use crate::registries::ExtensionRegistries;
use crate::settings::SnakeSettings;

const ALL_CAPABILITIES: &[&str] = &[
    "stats.read",
    "ui.register",
    "ui.update",
    "skin.register",
    "food.register",
    "spawn.register",
    "spawn.propose",
    "game.score",
    "game.resize",
    "game.speed",
    "diagnostics.log",
];
const SCORE_OVERLAY_ID: &str = "community.score-overlay";
const OVERLAY_STATE_KEY: &str = "score-overlay.session";
const OVERLAY_STATE_TYPE: &str = "OverlayState";
const OVERLAY_FOODS_FIELD: &str = "foods";

#[derive(Clone, Debug)]
pub enum ExtensionAction {
    Enable(PackageId),
    Disable(PackageId),
    Reload(PackageId),
}

#[derive(Clone, Debug)]
pub struct ExtensionView {
    pub packages: Vec<PackageInfo>,
    pub widgets: Vec<(String, String)>,
    pub selected_skin: Option<String>,
    pub selected_spawn_policy: Option<String>,
    pub toast: Option<String>,
    pub safe_mode: bool,
    pub ui_entries: usize,
    pub skin_entries: usize,
    pub food_entries: usize,
    pub spawn_entries: usize,
    pub skins: Vec<String>,
    pub spawn_policies: Vec<String>,
}

#[derive(Clone, Default)]
struct SharedEntitlements(Arc<RwLock<BTreeSet<EntitlementId>>>);

impl SharedEntitlements {
    fn replace(&self, values: impl IntoIterator<Item = EntitlementId>) {
        let mut current = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.clear();
        current.extend(values);
    }

    fn values(&self) -> BTreeSet<String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|value| value.as_str().to_owned())
            .collect()
    }
}

impl EntitlementResolver for SharedEntitlements {
    fn contains(&self, id: &EntitlementId) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id)
    }
}

#[derive(Clone, Default)]
struct SnakeHost;

impl generated::SnakeHost for SnakeHost {
    fn format_score(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        score: i32,
    ) -> Result<String, generated::HostError> {
        Ok(format!("Score: {score}"))
    }

    fn format_stats(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        score: i32,
        total_plays: i64,
        foods: i32,
    ) -> Result<String, generated::HostError> {
        Ok(format!(
            "Score {score} · Plays {total_plays} · Foods {foods}"
        ))
    }

    fn log(
        &mut self,
        _: &mut nexa_runtime::ResourceContext<'_>,
        message: &str,
    ) -> Result<i32, generated::HostError> {
        eprintln!("[nexa-snake] {message}");
        i32::try_from(message.len()).map_err(|_| generated::HostError("message too long".into()))
    }
}

pub struct SnakeExtensions {
    embed: NexaEmbed,
    registries: ExtensionRegistries,
    pending_actions: Vec<ExtensionAction>,
    safe_mode: bool,
    selected_skin: Option<String>,
    selected_spawn_policy: Option<String>,
    toast: Option<String>,
    entitlements: SharedEntitlements,
    settings_path: PathBuf,
}

impl SnakeExtensions {
    pub fn load(game: &SnakeGame) -> Result<Self, Box<dyn std::error::Error>> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let data_root = crate::persistence::default_data_root();
        Self::load_from(game, root, data_root)
    }

    pub fn load_from(
        game: &SnakeGame,
        root: impl Into<PathBuf>,
        data_root: impl Into<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let root = root.into();
        let data_root = data_root.into();
        let settings_path = data_root.join("settings.json");
        let settings = crate::persistence::load(&settings_path)?;
        let entitlements = SharedEntitlements::default();
        entitlements.replace(
            settings
                .entitlements
                .iter()
                .filter_map(|value| EntitlementId::new(value.clone()).ok()),
        );
        let mut embed = NexaEmbed::builder(generated::contract())
            .host_factory(|_: &nexa_embed::PackageContext| generated::registry(SnakeHost))
            .package_source(DirectorySource::new(
                SourceId::new("snake-builtin")?,
                root.join("packages/builtin"),
                source_policy(SourceCategory::Builtin)?,
            ))
            .package_source(DirectorySource::new(
                SourceId::new("snake-dlc")?,
                root.join("packages/dlc"),
                source_policy(SourceCategory::Dlc)?,
            ))
            .package_source(DirectorySource::new(
                SourceId::new("snake-mod")?,
                root.join("packages/mods"),
                source_policy(SourceCategory::Mod)?,
            ))
            .entitlements(entitlements.clone())
            .storage_dir(data_root.join("embed"))
            .development_mode(true)
            .require_export::<generated::OnEvent>()
            .build()?;
        embed.discover()?;
        embed.enable_defaults()?;
        let mut extensions = Self {
            embed,
            registries: ExtensionRegistries::default(),
            pending_actions: Vec::new(),
            safe_mode: false,
            selected_skin: settings.selected_skin,
            selected_spawn_policy: settings.selected_spawn_policy,
            toast: None,
            entitlements,
            settings_path,
        };
        let event = GameEvent::new(GameEventKind::PackageEnabled, game.snapshot());
        let enabled = extensions
            .embed
            .packages()
            .into_iter()
            .filter(|package| package.status == PackageStatus::Enabled)
            .map(|package| package.id)
            .collect::<Vec<_>>();
        for id in enabled {
            extensions.call_package(&id, &event, &mut None)?;
        }
        extensions.update_fallbacks();
        Ok(extensions)
    }

    pub fn handle_events(
        &mut self,
        events: Vec<GameEvent>,
        game: &mut SnakeGame,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for event in events {
            self.update_overlay_state(event.kind)?;
            let overlay_foods = self.overlay_foods();
            let outputs =
                self.embed
                    .dispatch_with::<generated::OnEvent>(|package| generated::OnEventArgs {
                        event: to_generated_event(
                            &event,
                            (package.id.as_str() == SCORE_OVERLAY_ID)
                                .then_some(overlay_foods)
                                .flatten(),
                        ),
                    });
            for output in outputs {
                match output {
                    Ok(output) => {
                        let commands = output
                            .value
                            .into_iter()
                            .map(from_generated_command)
                            .collect::<Vec<_>>();
                        self.apply_or_fault(&output.package_id, event.kind, commands, game)?;
                    }
                    Err(error) => self.toast = Some(error.to_string()),
                }
            }
            if event.kind == GameEventKind::FoodSpawnRequested && game.food().is_some() {
                if let Some((food_id, _)) = self
                    .registries
                    .foods
                    .select(usize::try_from(event.snapshot.foods).unwrap_or_default())
                {
                    game.set_food_kind(food_id);
                } else {
                    game.set_food_kind("classic");
                }
            }
        }
        self.update_fallbacks();
        Ok(())
    }

    pub fn tick(&mut self, _: &mut SnakeGame) -> Result<(), Box<dyn std::error::Error>> {
        self.embed.tick()?;
        let packages = self.embed.packages();
        let classic_rules = packages.iter().any(|package| {
            package.id.as_str() == "builtin.classic-rules"
                && package.status == PackageStatus::Enabled
        });
        self.safe_mode = !classic_rules
            || self.registries.spawn_policies.is_empty()
            || self.registries.skins.is_empty()
            || self.registries.ui.is_empty();
        Ok(())
    }

    pub fn apply_pending_actions(
        &mut self,
        game: &mut SnakeGame,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let actions = std::mem::take(&mut self.pending_actions);
        for action in actions {
            let (id, result, fault_enabled): (
                PackageId,
                Result<(), Box<dyn std::error::Error>>,
                bool,
            ) = match action {
                ExtensionAction::Enable(id) => {
                    let result = (|| {
                        self.embed.enable(&id)?;
                        if id.as_str() == SCORE_OVERLAY_ID {
                            self.set_overlay_foods(0)?;
                        }
                        let event = GameEvent::new(GameEventKind::PackageEnabled, game.snapshot());
                        self.call_package(&id, &event, &mut Some(game))
                    })();
                    (id, result, true)
                }
                ExtensionAction::Disable(id) => {
                    let result = self
                        .embed
                        .disable(&id)
                        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>);
                    if result.is_ok() {
                        self.registries.remove_owner(&id);
                    }
                    (id, result, false)
                }
                ExtensionAction::Reload(id) => {
                    let result = self
                        .embed
                        .reload(&id)
                        .map_err(|error| Box::new(error) as Box<dyn std::error::Error>);
                    (id, result, false)
                }
            };
            if let Err(error) = result {
                if fault_enabled && self.embed.status(&id) == Some(PackageStatus::Enabled) {
                    let _ = self.embed.fault(&id, error.to_string());
                }
                if self.embed.status(&id) != Some(PackageStatus::Enabled) {
                    self.registries.remove_owner(&id);
                }
                self.toast = Some(self.action_error_message(&id, error.as_ref()));
            }
        }
        self.update_fallbacks();
        Ok(())
    }

    fn call_package(
        &mut self,
        id: &PackageId,
        event: &GameEvent,
        game: &mut Option<&mut SnakeGame>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let output = self.embed.call::<generated::OnEvent>(
            id,
            &generated::OnEventArgs {
                event: to_generated_event(
                    event,
                    (id.as_str() == SCORE_OVERLAY_ID)
                        .then(|| self.overlay_foods())
                        .flatten(),
                ),
            },
        )?;
        let commands = output
            .value
            .into_iter()
            .map(from_generated_command)
            .collect::<Vec<_>>();
        if let Some(game) = game.as_deref_mut() {
            self.apply_batch(id, event.kind, commands, game)?;
        } else {
            let mut placeholder = SnakeGame::new(event.snapshot.total_plays);
            self.apply_batch(id, event.kind, commands, &mut placeholder)?;
        }
        Ok(())
    }

    fn apply_batch(
        &mut self,
        owner: &PackageId,
        event: GameEventKind,
        commands: Vec<SnakeCommand>,
        game: &mut SnakeGame,
    ) -> Result<(), String> {
        let package = self
            .embed
            .packages()
            .into_iter()
            .find(|package| package.id == *owner)
            .ok_or_else(|| "package disappeared during dispatch".to_owned())?;
        for command in &commands {
            validate_command(&package.capabilities, event, command, game)?;
            if let SnakeCommand::SetWidgetText { local_id, .. } = command
                && !self.registries.ui.owns(owner, local_id)
            {
                return Err("widget is not owned by package".into());
            }
        }
        for command in commands {
            match command {
                SnakeCommand::RegisterWidget(local_id) => {
                    self.registries.ui.register(owner, local_id);
                }
                SnakeCommand::SetWidgetText { local_id, text } => {
                    let updated = self.registries.ui.set_text(owner, &local_id, text);
                    debug_assert!(updated);
                }
                SnakeCommand::RegisterSkin(local_id) => {
                    self.registries.skins.register(owner, local_id);
                }
                SnakeCommand::RegisterFood(definition) => {
                    self.registries.foods.register(owner, definition);
                }
                SnakeCommand::RegisterSpawnPolicy(local_id) => {
                    self.registries.spawn_policies.register(owner, local_id);
                }
                SnakeCommand::AddScore(delta) => game.add_score(delta),
                SnakeCommand::ResizeSnake(delta) => game.resize(delta),
                SnakeCommand::SetSpeed(delta) => game.set_speed_delta(delta),
                SnakeCommand::ProposeFoodSpawn(cell) => {
                    if !game.propose_food(cell) {
                        self.toast = Some("invalid spawn proposal; safe spawn used".into());
                    }
                }
                SnakeCommand::ShowToast(message) => self.toast = Some(message),
            }
        }
        Ok(())
    }

    fn apply_or_fault(
        &mut self,
        owner: &PackageId,
        event: GameEventKind,
        commands: Vec<SnakeCommand>,
        game: &mut SnakeGame,
    ) -> Result<(), nexa_embed::EmbedError> {
        if let Err(error) = self.apply_batch(owner, event, commands, game) {
            self.embed.fault(owner, error.clone())?;
            self.registries.remove_owner(owner);
            self.toast = Some(format!("{owner} faulted: {error}"));
        }
        Ok(())
    }

    pub fn queue_enable(&mut self, id: PackageId) {
        self.pending_actions.push(ExtensionAction::Enable(id));
    }

    pub fn queue_disable(&mut self, id: PackageId) {
        self.pending_actions.push(ExtensionAction::Disable(id));
    }

    pub fn queue_reload(&mut self, id: PackageId) {
        self.pending_actions.push(ExtensionAction::Reload(id));
    }

    pub fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(message.into());
    }

    pub fn select_skin(&mut self, id: &str) -> bool {
        if self.registries.skins.contains(id) {
            self.selected_skin = Some(id.to_owned());
            true
        } else {
            false
        }
    }

    pub fn select_spawn_policy(&mut self, id: &str) -> bool {
        if self.registries.spawn_policies.contains(id) {
            self.selected_spawn_policy = Some(id.to_owned());
            true
        } else {
            false
        }
    }

    pub fn set_entitlements(
        &mut self,
        values: impl IntoIterator<Item = EntitlementId>,
    ) -> Result<(), nexa_embed::EmbedError> {
        let before = self.embed.packages();
        self.entitlements.replace(values);
        self.embed.refresh_entitlements()?;
        for package in before {
            if package.status == PackageStatus::Enabled
                && self.embed.status(&package.id) != Some(PackageStatus::Enabled)
            {
                self.registries.remove_owner(&package.id);
            }
        }
        self.update_fallbacks();
        Ok(())
    }

    #[must_use]
    pub fn packages(&self) -> Vec<PackageInfo> {
        self.embed.packages()
    }

    #[must_use]
    pub fn view(&self) -> ExtensionView {
        ExtensionView {
            packages: self.embed.packages(),
            widgets: self
                .registries
                .ui
                .values()
                .map(|entry| (entry.local_id.clone(), entry.value.clone()))
                .collect(),
            selected_skin: self.selected_skin.clone(),
            selected_spawn_policy: self.selected_spawn_policy.clone(),
            toast: self.toast.clone(),
            safe_mode: self.safe_mode,
            ui_entries: self.registries.ui.len(),
            skin_entries: self.registries.skins.len(),
            food_entries: self.registries.foods.len(),
            spawn_entries: self.registries.spawn_policies.len(),
            skins: self.registries.skins.ids(),
            spawn_policies: self.registries.spawn_policies.ids(),
        }
    }

    #[must_use]
    pub fn health(&self) -> nexa_embed::EmbedHealth {
        self.embed.health()
    }

    pub fn shutdown(&mut self, total_plays: i64) -> Result<(), Box<dyn std::error::Error>> {
        self.save_settings(total_plays)?;
        self.embed.shutdown()?;
        Ok(())
    }

    fn update_fallbacks(&mut self) {
        if self
            .selected_skin
            .as_ref()
            .is_some_and(|id| !self.registries.skins.contains(id))
        {
            self.selected_skin = self.registries.skins.first_id();
        }
        if self.selected_skin.is_none() {
            self.selected_skin = self.registries.skins.first_id();
        }
        if self
            .selected_spawn_policy
            .as_ref()
            .is_some_and(|id| !self.registries.spawn_policies.contains(id))
        {
            self.selected_spawn_policy = self.registries.spawn_policies.first_id();
        }
        if self.selected_spawn_policy.is_none() {
            self.selected_spawn_policy = self.registries.spawn_policies.first_id();
        }
    }

    fn action_error_message(
        &self,
        id: &PackageId,
        error: &(dyn std::error::Error + 'static),
    ) -> String {
        let name = self
            .embed
            .packages()
            .into_iter()
            .find(|package| package.id == *id)
            .map_or_else(|| id.to_string(), |package| package.name);
        if matches!(
            error.downcast_ref::<nexa_embed::EmbedError>(),
            Some(nexa_embed::EmbedError::RequiredPackage(_))
        ) {
            format!("{name} is required and cannot be disabled")
        } else {
            format!("{name}: {error}")
        }
    }

    fn save_settings(&self, total_plays: i64) -> Result<(), std::io::Error> {
        let settings = SnakeSettings {
            enabled_packages: self
                .embed
                .packages()
                .into_iter()
                .filter(|package| package.status == PackageStatus::Enabled)
                .map(|package| package.id.as_str().to_owned())
                .collect(),
            selected_skin: self.selected_skin.clone(),
            selected_spawn_policy: self.selected_spawn_policy.clone(),
            entitlements: self.entitlements.values(),
            total_plays,
        };
        crate::persistence::save(&self.settings_path, &settings)
    }

    fn update_overlay_state(
        &mut self,
        event: GameEventKind,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let id = PackageId::new(SCORE_OVERLAY_ID)?;
        if self.embed.status(&id) != Some(PackageStatus::Enabled) {
            return Ok(());
        }
        match event {
            GameEventKind::PackageEnabled | GameEventKind::GameStarted => {
                self.set_overlay_foods(0)?;
            }
            GameEventKind::FoodEaten => {
                let next = self.overlay_foods().unwrap_or_default().saturating_add(1);
                self.set_overlay_foods(next)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn set_overlay_foods(&mut self, value: i32) -> Result<(), nexa_embed::EmbedError> {
        let id = PackageId::new(SCORE_OVERLAY_ID).expect("static package ID is valid");
        self.embed.set_state_i32(
            &id,
            OVERLAY_STATE_KEY,
            OVERLAY_STATE_TYPE,
            1,
            OVERLAY_FOODS_FIELD,
            value,
        )
    }

    fn overlay_foods(&self) -> Option<i32> {
        let id = PackageId::new(SCORE_OVERLAY_ID).expect("static package ID is valid");
        self.embed
            .state_i32(
                &id,
                OVERLAY_STATE_KEY,
                OVERLAY_STATE_TYPE,
                OVERLAY_FOODS_FIELD,
            )
            .ok()
            .flatten()
    }
}

#[derive(Clone, Copy)]
enum SourceCategory {
    Builtin,
    Dlc,
    Mod,
}

fn source_policy(category: SourceCategory) -> Result<PackagePolicy, nexa_embed::ManifestError> {
    let all = CapabilitySet::new(
        ALL_CAPABILITIES
            .iter()
            .map(|value| CapabilityId::new(*value))
            .collect::<Result<Vec<_>, _>>()?,
    );
    let (trust, activation, entitlement) = match category {
        SourceCategory::Builtin => (
            TrustLevel::FirstParty,
            ActivationSet::new([ActivationPolicy::Required, ActivationPolicy::DefaultEnabled]),
            false,
        ),
        SourceCategory::Dlc => (
            TrustLevel::FirstParty,
            ActivationSet::new([ActivationPolicy::UserControlled]),
            true,
        ),
        SourceCategory::Mod => (
            TrustLevel::Trusted,
            ActivationSet::new([ActivationPolicy::UserControlled]),
            false,
        ),
    };
    Ok(PackagePolicy {
        trust,
        capability_ceiling: all,
        allowed_activation: activation,
        max_packages: 32,
        runtime_limits: PackageRuntimeLimits {
            handler_fuel: 50_000,
            cumulative_budget: 50_000,
            ..PackageRuntimeLimits::default()
        },
        allow_entitlement: entitlement,
    })
}

fn to_generated_snapshot(snapshot: &GameSnapshot) -> generated::GameSnapshot {
    generated::GameSnapshot {
        stats: generated::GameStats {
            score: snapshot.score,
            total_plays: snapshot.total_plays,
            foods: snapshot.foods,
            snake_length: snapshot.snake_length,
        },
        width: snapshot.width,
        height: snapshot.height,
        food: snapshot.food.map(|cell| generated::FoodInstance {
            kind: snapshot.food_kind.clone(),
            cell: generated::Cell {
                x: cell.x,
                y: cell.y,
            },
        }),
        available: generated::AvailableCells {
            values: snapshot
                .available
                .iter()
                .map(|cell| generated::Cell {
                    x: cell.x,
                    y: cell.y,
                })
                .collect(),
        },
    }
}

fn to_generated_event(event: &GameEvent, foods_override: Option<i32>) -> generated::SnakeEvent {
    let mut snapshot = to_generated_snapshot(&event.snapshot);
    if let Some(foods) = foods_override {
        snapshot.stats.foods = foods;
    }
    match event.kind {
        GameEventKind::PackageEnabled => generated::SnakeEvent::PackageEnabled(snapshot),
        GameEventKind::GameStarted => generated::SnakeEvent::GameStarted(snapshot),
        GameEventKind::GameTick => generated::SnakeEvent::GameTick(snapshot),
        GameEventKind::ScoreChanged => generated::SnakeEvent::ScoreChanged(snapshot),
        GameEventKind::FoodSpawnRequested => generated::SnakeEvent::FoodSpawnRequested(snapshot),
        GameEventKind::FoodEaten => generated::SnakeEvent::FoodEaten(snapshot),
        GameEventKind::GameEnded => generated::SnakeEvent::GameEnded(snapshot),
        GameEventKind::SettingsChanged => generated::SnakeEvent::SettingsChanged(snapshot),
    }
}

fn from_generated_command(command: generated::SnakeCommand) -> SnakeCommand {
    match command {
        generated::SnakeCommand::RegisterWidget(value) => SnakeCommand::RegisterWidget(value),
        generated::SnakeCommand::SetWidgetText(value) => SnakeCommand::SetWidgetText {
            local_id: value.local_id,
            text: value.text,
        },
        generated::SnakeCommand::RegisterSkin(value) => SnakeCommand::RegisterSkin(value),
        generated::SnakeCommand::RegisterFood(value) => {
            SnakeCommand::RegisterFood(FoodDefinition {
                local_id: value.local_id,
                length_delta: value.length_delta,
                score_delta: value.score_delta,
                speed_delta: value.speed_delta,
            })
        }
        generated::SnakeCommand::RegisterSpawnPolicy(value) => {
            SnakeCommand::RegisterSpawnPolicy(value)
        }
        generated::SnakeCommand::AddScore(value) => SnakeCommand::AddScore(value),
        generated::SnakeCommand::ResizeSnake(value) => SnakeCommand::ResizeSnake(value),
        generated::SnakeCommand::SetSpeed(value) => SnakeCommand::SetSpeed(value),
        generated::SnakeCommand::ProposeFoodSpawn(value) => SnakeCommand::ProposeFoodSpawn(Cell {
            x: value.x,
            y: value.y,
        }),
        generated::SnakeCommand::ShowToast(value) => SnakeCommand::ShowToast(value),
    }
}

fn validate_command(
    capabilities: &CapabilitySet,
    event: GameEventKind,
    command: &SnakeCommand,
    _: &SnakeGame,
) -> Result<(), String> {
    let (capability, allowed) = match command {
        SnakeCommand::RegisterWidget(_) => ("ui.register", event == GameEventKind::PackageEnabled),
        SnakeCommand::SetWidgetText { .. } => (
            "ui.update",
            matches!(event, GameEventKind::GameTick | GameEventKind::ScoreChanged),
        ),
        SnakeCommand::RegisterSkin(_) => ("skin.register", event == GameEventKind::PackageEnabled),
        SnakeCommand::RegisterFood(_) => ("food.register", event == GameEventKind::PackageEnabled),
        SnakeCommand::RegisterSpawnPolicy(_) => {
            ("spawn.register", event == GameEventKind::PackageEnabled)
        }
        SnakeCommand::AddScore(_) => ("game.score", event == GameEventKind::FoodEaten),
        SnakeCommand::ResizeSnake(_) => ("game.resize", event == GameEventKind::FoodEaten),
        SnakeCommand::SetSpeed(_) => ("game.speed", event == GameEventKind::FoodEaten),
        SnakeCommand::ProposeFoodSpawn(_) => {
            ("spawn.propose", event == GameEventKind::FoodSpawnRequested)
        }
        SnakeCommand::ShowToast(_) => ("diagnostics.log", true),
    };
    let capability = CapabilityId::new(capability).map_err(|error| error.to_string())?;
    if !capabilities.contains(&capability) {
        return Err(format!("missing capability {capability}"));
    }
    if !allowed {
        return Err(format!("command is not valid during {event:?}"));
    }
    match command {
        SnakeCommand::RegisterWidget(id)
        | SnakeCommand::RegisterSkin(id)
        | SnakeCommand::RegisterSpawnPolicy(id)
            if !valid_local_id(id) =>
        {
            Err("invalid local item id".into())
        }
        SnakeCommand::RegisterFood(definition) if !valid_local_id(&definition.local_id) => {
            Err("invalid local food id".into())
        }
        SnakeCommand::ResizeSnake(delta) if !(-8..=8).contains(delta) => {
            Err("resize delta exceeds policy".into())
        }
        SnakeCommand::SetSpeed(delta) if !(-200..=200).contains(delta) => {
            Err("speed delta exceeds policy".into())
        }
        _ => Ok(()),
    }
}

fn valid_local_id(value: &str) -> bool {
    !value.is_empty()
        && !value.contains(['/', '\\', ':'])
        && !value.contains("..")
        && !value.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(label: &str) -> (SnakeGame, SnakeExtensions, PathBuf) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let data = std::env::temp_dir().join(format!("nexa-snake-{label}-{}", std::process::id()));
        if data.exists() {
            std::fs::remove_dir_all(&data).expect("remove stale Snake test data");
        }
        let game = SnakeGame::new(7);
        let extensions =
            SnakeExtensions::load_from(&game, root, &data).expect("load Snake extensions");
        (game, extensions, data)
    }

    fn id(value: &str) -> PackageId {
        PackageId::new(value).expect("static package ID")
    }

    fn enable(extensions: &mut SnakeExtensions, game: &mut SnakeGame, package: &str) {
        extensions.queue_enable(id(package));
        extensions
            .apply_pending_actions(game)
            .expect("enable package");
    }

    #[test]
    fn all_real_packages_enable_and_owned_registries_clean_up() {
        let (mut game, mut extensions, _) = harness("packages");
        extensions
            .set_entitlements([EntitlementId::new("official.food-chaos").expect("entitlement")])
            .expect("grant entitlement");
        for package in [
            "official.food-chaos",
            "community.score-overlay",
            "community.neon-skin",
            "community.weird-foods",
            "community.corner-spawn",
        ] {
            enable(&mut extensions, &mut game, package);
        }
        extensions.tick(&mut game).expect("tick");
        let view = extensions.view();
        assert!(!view.safe_mode);
        assert_eq!(view.packages.len(), 9);
        assert_eq!(view.ui_entries, 2);
        assert_eq!(view.skin_entries, 2);
        assert_eq!(view.food_entries, 6);
        assert_eq!(view.spawn_entries, 2);

        assert!(extensions.select_skin("community.neon-skin:neon"));
        assert!(extensions.select_spawn_policy("community.corner-spawn:corner"));
        extensions.queue_disable(id("community.neon-skin"));
        extensions.queue_disable(id("community.corner-spawn"));
        extensions
            .apply_pending_actions(&mut game)
            .expect("disable selected extensions");
        let view = extensions.view();
        assert_eq!(view.skin_entries, 1);
        assert_eq!(view.spawn_entries, 1);
        assert_eq!(
            view.selected_skin.as_deref(),
            Some("builtin.default-skin:classic")
        );
        assert_eq!(
            view.selected_spawn_policy.as_deref(),
            Some("builtin.classic-spawn:classic")
        );
    }

    #[test]
    fn overlay_state_survives_restart_reload_and_updates_widget() {
        let (mut game, mut extensions, _) = harness("overlay");
        enable(&mut extensions, &mut game, "community.score-overlay");
        let eaten = GameEvent::new(GameEventKind::FoodEaten, game.snapshot());
        extensions
            .handle_events(vec![eaten], &mut game)
            .expect("dispatch food event");
        assert_eq!(extensions.overlay_foods(), Some(1));
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::GameTick, game.snapshot())],
                &mut game,
            )
            .expect("update overlay");
        let view = extensions.view();
        assert!(
            view.widgets
                .iter()
                .any(|(_, text)| text.contains("Foods 1")),
            "{view:?}"
        );

        extensions.queue_reload(id("community.score-overlay"));
        extensions
            .apply_pending_actions(&mut game)
            .expect("restart reload overlay");
        assert_eq!(extensions.overlay_foods(), Some(1));
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::GameTick, game.snapshot())],
                &mut game,
            )
            .expect("update reloaded overlay");
        let view = extensions.view();
        assert!(
            view.widgets
                .iter()
                .any(|(_, text)| text.contains("Foods 1")),
            "{view:?}"
        );
        assert_eq!(extensions.overlay_foods(), Some(1));
    }

    #[test]
    fn command_batches_are_atomic_capability_checked_and_bounded() {
        let (mut game, mut extensions, _) = harness("commands");
        extensions
            .set_entitlements([EntitlementId::new("official.food-chaos").expect("entitlement")])
            .expect("grant entitlement");
        enable(&mut extensions, &mut game, "official.food-chaos");
        let before_score = game.score();
        let before_length = game.snapshot().snake_length;
        assert!(
            extensions
                .apply_batch(
                    &id("official.food-chaos"),
                    GameEventKind::FoodEaten,
                    vec![SnakeCommand::AddScore(5), SnakeCommand::ResizeSnake(99)],
                    &mut game,
                )
                .is_err()
        );
        assert_eq!(game.score(), before_score);
        assert_eq!(game.snapshot().snake_length, before_length);
        assert!(
            extensions
                .apply_batch(
                    &id("builtin.classic-hud"),
                    GameEventKind::FoodEaten,
                    vec![SnakeCommand::AddScore(5)],
                    &mut game,
                )
                .is_err()
        );
        for _ in 0..16 {
            game.resize(-1);
        }
        assert_eq!(game.snapshot().snake_length, 2);
    }

    #[test]
    fn named_food_packages_apply_grow_shrink_speed_and_score_effects() {
        let (mut game, mut extensions, _) = harness("foods");
        extensions
            .set_entitlements([EntitlementId::new("official.food-chaos").expect("entitlement")])
            .expect("grant entitlement");
        enable(&mut extensions, &mut game, "official.food-chaos");
        enable(&mut extensions, &mut game, "community.weird-foods");
        assert!(game.propose_food(Cell { x: 0, y: 0 }));

        game.set_food_kind("official.food-chaos:golden");
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::FoodEaten, game.snapshot())],
                &mut game,
            )
            .expect("golden food");
        assert_eq!(game.score(), 20);
        let _ = game.step(crate::game::GameInput::default());
        let _ = game.step(crate::game::GameInput::default());
        assert_eq!(game.snapshot().snake_length, 5);

        game.set_food_kind("official.food-chaos:poison");
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::FoodEaten, game.snapshot())],
                &mut game,
            )
            .expect("poison food");
        assert_eq!(game.snapshot().snake_length, 4);

        game.set_food_kind("official.food-chaos:speed");
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::FoodEaten, game.snapshot())],
                &mut game,
            )
            .expect("speed food");
        assert_eq!(game.speed_millis(), 120);

        game.set_food_kind("community.weird-foods:shrink");
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::FoodEaten, game.snapshot())],
                &mut game,
            )
            .expect("shrink food");
        assert_eq!(game.snapshot().snake_length, 3);

        game.set_food_kind("community.weird-foods:score-bomb");
        extensions
            .handle_events(
                vec![GameEvent::new(GameEventKind::FoodEaten, game.snapshot())],
                &mut game,
            )
            .expect("score bomb");
        assert_eq!(game.score(), 70);
    }

    #[test]
    fn invalid_mod_fault_isolated_and_removes_owned_registry() {
        let (mut game, mut extensions, _) = harness("fault");
        enable(&mut extensions, &mut game, "community.neon-skin");
        let owner = id("community.neon-skin");
        let before = game.score();
        extensions
            .apply_or_fault(
                &owner,
                GameEventKind::FoodEaten,
                vec![SnakeCommand::AddScore(999)],
                &mut game,
            )
            .expect("isolate invalid Mod");
        assert_eq!(game.score(), before);
        assert_eq!(
            extensions.embed.status(&owner),
            Some(PackageStatus::Faulted)
        );
        assert!(
            !extensions
                .view()
                .skins
                .iter()
                .any(|skin| skin.starts_with("community.neon-skin:"))
        );
        let events = game.step(crate::game::GameInput::default());
        extensions
            .handle_events(events, &mut game)
            .expect("game continues after Mod fault");
    }

    #[test]
    fn locked_dlc_unlocks_and_safe_mode_survives_package_faults() {
        let (mut game, mut extensions, _) = harness("safe");
        let dlc = id("official.food-chaos");
        assert_eq!(extensions.embed.status(&dlc), Some(PackageStatus::Locked));
        extensions
            .set_entitlements([EntitlementId::new("official.food-chaos").expect("entitlement")])
            .expect("grant entitlement");
        assert_eq!(extensions.embed.status(&dlc), Some(PackageStatus::Disabled));
        enable(&mut extensions, &mut game, "official.food-chaos");

        for package in [
            "builtin.classic-rules",
            "builtin.classic-hud",
            "builtin.default-skin",
            "builtin.classic-spawn",
        ] {
            let package = id(package);
            extensions
                .embed
                .fault(&package, "test fault")
                .expect("fault package");
            extensions.registries.remove_owner(&package);
        }
        extensions.tick(&mut game).expect("tick safe mode");
        assert!(extensions.view().safe_mode);
        for _ in 0..64 {
            let events = game.step(crate::game::GameInput::default());
            extensions
                .handle_events(events, &mut game)
                .expect("safe-mode event");
        }
        assert!(game.total_plays() > 0);
    }

    #[test]
    fn invalid_spawn_falls_back_and_total_plays_persist() {
        let (mut game, mut extensions, data) = harness("persistence");
        assert!(
            extensions
                .apply_batch(
                    &id("builtin.classic-spawn"),
                    GameEventKind::FoodSpawnRequested,
                    vec![SnakeCommand::ProposeFoodSpawn(Cell { x: -1, y: -1 })],
                    &mut game,
                )
                .is_ok()
        );
        let first = game.step(crate::game::GameInput::default());
        assert!(
            first
                .iter()
                .any(|event| event.kind == GameEventKind::FoodSpawnRequested)
        );
        let _ = game.step(crate::game::GameInput::default());
        assert!(game.food().is_some());

        extensions
            .shutdown(game.total_plays())
            .expect("shutdown and save");
        let restored =
            crate::persistence::load(&data.join("settings.json")).expect("reload settings");
        assert_eq!(restored.total_plays, game.total_plays());
    }
}
