fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("smoke") => {
            snake_game::headless::smoke()?;
            println!("snake headless smoke: PASS");
        }
        Some("stress") => {
            let report = snake_game::headless::stress()?;
            println!("{}", serde_json::to_string(&report)?);
        }
        Some("bench") => {
            let report = snake_game::headless::bench()?;
            println!(
                "snake bench: PASS samples={} packages={} p95_us={} p99_us={}",
                report.samples,
                report.enabled_packages,
                report.p95.as_micros(),
                report.p99.as_micros()
            );
        }
        _ => return Err("usage: snake-headless smoke|stress|bench".into()),
    }
    Ok(())
}
