use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use nexa_embed::{EntitlementId, PackageId, PackageStatus};

use crate::{GameEvent, GameEventKind, GameInput, SnakeExtensions, SnakeGame};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BenchReport {
    pub samples: usize,
    pub enabled_packages: usize,
    pub p95: Duration,
    pub p99: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StressReport {
    schema: u32,
    steady_ticks: usize,
    disable_enable_cycles: usize,
    reload_cycles: usize,
    entitlement_cycles: usize,
    post_shutdown: ResourceSnapshot,
    resource_leaks: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
struct ResourceSnapshot {
    enabled_packages: u64,
    tasks: u64,
    scopes: u64,
    continuations: u64,
    scheduler_tokens: u64,
    requests: u64,
    completion_reservations: u64,
    tokens: u64,
    snapshots: u64,
    release_reservations: u64,
    queued_releases: u64,
    heap_objects: u64,
    state_objects: u64,
    retired_modules: u64,
    host_pending_completions: u64,
    host_pending_releases: u64,
}

impl ResourceSnapshot {
    fn from_health(health: nexa_embed::EngineHealth) -> Self {
        Self {
            enabled_packages: u64::try_from(health.enabled_packages).unwrap_or(u64::MAX),
            tasks: health.tasks,
            scopes: health.scopes,
            continuations: health.continuations,
            scheduler_tokens: health.scheduler_tokens,
            requests: health.requests,
            completion_reservations: health.completion_reservations,
            tokens: health.tokens,
            snapshots: health.snapshots,
            release_reservations: health.release_reservations,
            queued_releases: health.queued_releases,
            heap_objects: health.heap_objects,
            state_objects: health.state_objects,
            retired_modules: health.retired_modules,
            host_pending_completions: u64::try_from(health.host_pending_completions)
                .unwrap_or(u64::MAX),
            host_pending_releases: u64::try_from(health.host_pending_releases).unwrap_or(u64::MAX),
        }
    }

    fn total(self) -> u64 {
        [
            self.enabled_packages,
            self.tasks,
            self.scopes,
            self.continuations,
            self.scheduler_tokens,
            self.requests,
            self.completion_reservations,
            self.tokens,
            self.snapshots,
            self.release_reservations,
            self.queued_releases,
            self.heap_objects,
            self.state_objects,
            self.retired_modules,
            self.host_pending_completions,
            self.host_pending_releases,
        ]
        .into_iter()
        .fold(0_u64, u64::saturating_add)
    }
}

pub fn smoke() -> Result<(), Box<dyn std::error::Error>> {
    let (mut game, mut extensions) = all_packages("smoke", true)?;
    for _ in 0..512 {
        let events = game.step(GameInput::default());
        extensions.handle_events(events, &mut game)?;
        extensions.tick(&mut game)?;
    }
    let view = extensions.view();
    if view.packages.len() != 9
        || view.ui_entries < 2
        || view.skin_entries < 2
        || view.food_entries != 6
        || view.spawn_entries < 2
    {
        return Err(format!("incomplete package registration: {view:?}").into());
    }
    extensions.shutdown(game.total_plays())?;
    Ok(())
}

pub fn stress() -> Result<StressReport, Box<dyn std::error::Error>> {
    const STEADY_TICKS: usize = 1_024;
    const LIFECYCLE_CYCLES: usize = 100;

    let (mut game, mut extensions) = all_packages("stress", true)?;
    for _ in 0..128 {
        let events = game.step(GameInput::default());
        extensions.handle_events(events, &mut game)?;
        extensions.tick(&mut game)?;
    }
    let baseline = extensions.health();
    for _ in 0..STEADY_TICKS {
        let events = game.step(GameInput::default());
        extensions.handle_events(events, &mut game)?;
        extensions.tick(&mut game)?;
    }
    let after_ticks = extensions.health();
    assert_transient_resources(after_ticks)?;
    if after_ticks.enabled_packages != baseline.enabled_packages
        || after_ticks.state_objects != baseline.state_objects
    {
        return Err(format!(
            "runtime ownership grew: baseline={baseline:?}, after={after_ticks:?}, packages={:?}",
            extensions.packages()
        )
        .into());
    }

    let neon = package_id("community.neon-skin")?;
    for _ in 0..LIFECYCLE_CYCLES {
        extensions.queue_disable(neon.clone());
        extensions.apply_pending_actions(&mut game)?;
        extensions.queue_enable(neon.clone());
        extensions.apply_pending_actions(&mut game)?;
        extensions.tick(&mut game)?;
    }

    let overlay = package_id("community.score-overlay")?;
    for _ in 0..LIFECYCLE_CYCLES {
        extensions.queue_reload(overlay.clone());
        extensions.apply_pending_actions(&mut game)?;
        extensions.tick(&mut game)?;
        if extensions
            .packages()
            .iter()
            .find(|package| package.id == overlay)
            .is_none_or(|package| package.status != PackageStatus::Enabled)
        {
            return Err("overlay failed during reload stress".into());
        }
    }

    let entitlement = EntitlementId::new("official.food-chaos")?;
    let dlc = package_id("official.food-chaos")?;
    for _ in 0..LIFECYCLE_CYCLES {
        extensions.set_entitlements([])?;
        if extensions
            .packages()
            .iter()
            .find(|package| package.id == dlc)
            .is_none_or(|package| package.status != PackageStatus::Locked)
        {
            return Err("DLC did not lock".into());
        }
        extensions.set_entitlements([entitlement.clone()])?;
        extensions.queue_enable(dlc.clone());
        extensions.apply_pending_actions(&mut game)?;
    }

    let final_health = extensions.health();
    assert_transient_resources(final_health)?;
    let view = extensions.view();
    if view.ui_entries != 2
        || view.skin_entries != 2
        || view.food_entries != 6
        || view.spawn_entries != 2
    {
        return Err(format!("registry leak or loss after stress: {view:?}").into());
    }
    extensions.shutdown(game.total_plays())?;
    let post_shutdown = ResourceSnapshot::from_health(extensions.health());
    let resource_leaks = post_shutdown.total();
    if resource_leaks != 0 {
        return Err(format!("shutdown left runtime resources: {post_shutdown:?}").into());
    }
    Ok(StressReport {
        schema: 1,
        steady_ticks: STEADY_TICKS,
        disable_enable_cycles: LIFECYCLE_CYCLES,
        reload_cycles: LIFECYCLE_CYCLES,
        entitlement_cycles: LIFECYCLE_CYCLES,
        post_shutdown,
        resource_leaks,
    })
}

pub fn bench() -> Result<BenchReport, Box<dyn std::error::Error>> {
    let (mut game, mut extensions) = all_packages("bench", false)?;
    let event = GameEvent::new(GameEventKind::GameTick, game.snapshot());
    for _ in 0..32 {
        extensions.handle_events(vec![event.clone()], &mut game)?;
        extensions.tick(&mut game)?;
    }
    let mut samples = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let started = Instant::now();
        extensions.handle_events(vec![event.clone()], &mut game)?;
        samples.push(started.elapsed());
        extensions.tick(&mut game)?;
    }
    samples.sort_unstable();
    let p95 = samples[949];
    let p99 = samples[989];
    let report = BenchReport {
        samples: samples.len(),
        enabled_packages: extensions.health().enabled_packages,
        p95,
        p99,
    };
    if report.enabled_packages != 8
        || report.p95 > Duration::from_millis(4)
        || report.p99 > Duration::from_millis(8)
    {
        return Err(format!("Snake dispatch budget failed: {report:?}").into());
    }
    extensions.shutdown(game.total_plays())?;
    Ok(report)
}

fn all_packages(
    label: &str,
    include_dlc: bool,
) -> Result<(SnakeGame, SnakeExtensions), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let data = std::env::temp_dir().join(format!(
        "nexa-snake-headless-{label}-{}-{nonce}",
        std::process::id()
    ));
    let mut game = SnakeGame::new(0);
    let mut extensions = SnakeExtensions::load_from(&game, root, data)?;
    if include_dlc {
        extensions.set_entitlements([EntitlementId::new("official.food-chaos")?])?;
    }
    for package in extensions.packages() {
        if package.status == PackageStatus::Disabled
            || (include_dlc && package.id.as_str() == "official.food-chaos")
        {
            extensions.queue_enable(package.id);
        }
    }
    extensions.apply_pending_actions(&mut game)?;
    Ok((game, extensions))
}

fn package_id(value: &str) -> Result<PackageId, Box<dyn std::error::Error>> {
    Ok(PackageId::new(value)?)
}

fn assert_transient_resources(
    health: nexa_embed::EngineHealth,
) -> Result<(), Box<dyn std::error::Error>> {
    if health.tasks != 0
        || health.continuations != 0
        || health.scheduler_tokens != 0
        || health.requests != 0
        || health.completion_reservations != 0
        || health.tokens != 0
        || health.snapshots != 0
        || health.release_reservations != 0
        || health.queued_releases != 0
        || health.retired_modules != 0
        || health.host_pending_completions != 0
        || health.host_pending_releases != 0
    {
        return Err(format!("transient runtime resources remain: {health:?}").into());
    }
    Ok(())
}
