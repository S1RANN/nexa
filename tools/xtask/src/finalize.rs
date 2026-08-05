use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::DynError;

pub(crate) const M5_FINALIZE_STEPS: &[&str] = &[
    "release authority and publication preflight",
    "cargo fmt",
    "workspace clippy",
    "workspace tests and receipt",
    "workspace doc tests",
    "workspace documentation",
    "workspace test receipt validation",
    "non-workspace correctness evidence",
    "M4 and M4R1 specialized evidence",
    "M5 feature-specific gates",
    "profiler overhead and canonical HEAD aggregate",
    "live baseline comparison",
    "V8 comparison",
    "M5 final performance report",
    "repository audit",
    "terminal receipt validation and publication",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FinalizeM5Options {
    pub(crate) dry_run: bool,
    pub(crate) force_bench: bool,
    pub(crate) refresh_baseline: bool,
}

impl FinalizeM5Options {
    pub(crate) fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Self, DynError> {
        let mut options = Self::default();
        for argument in arguments {
            match argument.as_str() {
                "--dry-run" if !options.dry_run => options.dry_run = true,
                "--force-bench" if !options.force_bench => options.force_bench = true,
                "--refresh-baseline" if !options.refresh_baseline => {
                    options.refresh_baseline = true;
                }
                "--dry-run" | "--force-bench" | "--refresh-baseline" => {
                    return Err(format!("duplicate finalize-m5 option `{argument}`").into());
                }
                _ => return Err(format!("unknown finalize-m5 option `{argument}`").into()),
            }
        }
        Ok(options)
    }

    pub(crate) fn print_plan(self) {
        println!("finalize-m5 execution plan:");
        for (index, step) in M5_FINALIZE_STEPS.iter().enumerate() {
            println!("{:>2}. {step}", index + 1);
        }
        println!(
            "options: force_bench={}, refresh_baseline={}",
            self.force_bench, self.refresh_baseline
        );
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StepTiming {
    name: String,
    elapsed_milliseconds: u64,
    status: &'static str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimingReceipt<'a> {
    schema: u32,
    command: &'static str,
    implementation_commit: &'a str,
    started_unix_milliseconds: u64,
    elapsed_milliseconds: u64,
    steps: &'a [StepTiming],
    status: &'static str,
}

pub(crate) struct FinalizeTimings {
    output: PathBuf,
    implementation_commit: String,
    started_at: Instant,
    started_unix_milliseconds: u64,
    steps: Vec<StepTiming>,
}

impl FinalizeTimings {
    pub(crate) fn new(root: &Path, implementation_commit: String) -> Self {
        Self {
            output: root.join("target/nexa-artifacts/m5-finalize/timings.json"),
            implementation_commit,
            started_at: Instant::now(),
            started_unix_milliseconds: unix_milliseconds(SystemTime::now()),
            steps: Vec::with_capacity(M5_FINALIZE_STEPS.len()),
        }
    }

    pub(crate) fn run<T>(
        &mut self,
        name: impl Into<String>,
        action: impl FnOnce() -> Result<T, DynError>,
    ) -> Result<T, DynError> {
        let mut timer = StepTimer::new(name.into(), &mut self.steps);
        let result = action();
        timer.status = if result.is_ok() { "PASS" } else { "FAIL" };
        result
    }

    pub(crate) fn write(self, passed: bool) -> Result<(), DynError> {
        let elapsed = self.started_at.elapsed();
        let receipt = TimingReceipt {
            schema: 1,
            command: "cargo xtask finalize-m5",
            implementation_commit: &self.implementation_commit,
            started_unix_milliseconds: self.started_unix_milliseconds,
            elapsed_milliseconds: duration_milliseconds(elapsed),
            steps: &self.steps,
            status: if passed { "PASS" } else { "FAIL" },
        };
        fs::create_dir_all(
            self.output
                .parent()
                .ok_or("M5 timing receipt has no parent directory")?,
        )?;
        let temporary = self.output.with_extension("json.tmp");
        fs::write(
            &temporary,
            format!("{}\n", serde_json::to_string_pretty(&receipt)?),
        )?;
        fs::rename(temporary, &self.output)?;

        eprintln!("finalize-m5 timings:");
        for step in &self.steps {
            eprintln!(
                "  {:>9.3}s  {:<4}  {}",
                Duration::from_millis(step.elapsed_milliseconds).as_secs_f64(),
                step.status,
                step.name
            );
        }
        eprintln!(
            "  {:>9.3}s  {:<4}  total",
            elapsed.as_secs_f64(),
            receipt.status
        );
        Ok(())
    }
}

struct StepTimer<'a> {
    name: Option<String>,
    started_at: Instant,
    status: &'static str,
    sink: &'a mut Vec<StepTiming>,
}

impl<'a> StepTimer<'a> {
    fn new(name: String, sink: &'a mut Vec<StepTiming>) -> Self {
        Self {
            name: Some(name),
            started_at: Instant::now(),
            status: "FAIL",
            sink,
        }
    }
}

impl Drop for StepTimer<'_> {
    fn drop(&mut self) {
        self.sink.push(StepTiming {
            name: self.name.take().expect("step timing name is present"),
            elapsed_milliseconds: duration_milliseconds(self.started_at.elapsed()),
            status: self.status,
        });
    }
}

fn duration_milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_milliseconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(duration_milliseconds)
        .unwrap_or_default()
}

pub(crate) fn rustc_version() -> Result<String, DynError> {
    let output = Command::new("rustc").arg("-Vv").output()?;
    if !output.status.success() {
        return Err(format!("rustc -Vv failed with {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::FinalizeM5Options;

    #[test]
    fn finalize_options_are_strict_and_order_independent() {
        let options = FinalizeM5Options::parse([
            "--refresh-baseline".to_owned(),
            "--force-bench".to_owned(),
            "--dry-run".to_owned(),
        ])
        .expect("valid options");
        assert!(options.dry_run);
        assert!(options.force_bench);
        assert!(options.refresh_baseline);
        assert!(
            FinalizeM5Options::parse(["--dry-run".to_owned(), "--dry-run".to_owned()]).is_err()
        );
        assert!(FinalizeM5Options::parse(["--unknown".to_owned()]).is_err());
    }
}
