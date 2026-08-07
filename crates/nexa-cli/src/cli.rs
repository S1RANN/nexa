use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::DiagnosticFormat;

const REMOVED_TOP_LEVEL_COMMANDS: &[&str] = &[
    "trace",
    "verify",
    "migrate-check",
    "model-check",
    "model-replay",
    "diagnostic-corpus-check",
    "fixture-check",
    "baseline-check",
    "machine-check",
];

#[derive(Clone, Debug, Parser)]
#[command(
    name = "nexa",
    version,
    about = "Nexa language toolchain and runtime",
    args_conflicts_with_subcommands = true,
    subcommand_precedence_over_arg = true,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Diagnostic output protocol used by commands and usage errors.
    #[arg(long, value_enum, global = true, default_value_t = DiagnosticFormat::Human)]
    pub(crate) diagnostic_format: DiagnosticFormat,

    /// Fuel available to a top-level script invocation.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) fuel: Option<u64>,

    /// Verifier/runtime limits used by a top-level script invocation.
    #[arg(long)]
    pub(crate) limits_file: Option<PathBuf>,

    /// Source file or package followed by its arguments.
    #[arg(
        value_name = "SCRIPT_OR_ARG",
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub(crate) script_and_args: Vec<OsString>,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Clone, Debug, Subcommand)]
pub(crate) enum Command {
    /// Analyze a source file, package, or project.
    #[command(
        after_help = "Examples:\n  nexa check game.nexa\n  nexa check --project nexa.dev.toml"
    )]
    Check(CheckArgs),

    /// Compile an Application to Bytecode v7.
    #[command(
        after_help = "Examples:\n  nexa build game.nexa -o game.nxb\n  nexa build --project nexa.dev.toml"
    )]
    Build(BuildArgs),

    /// Discover and execute canonical @test functions.
    #[command(
        after_help = "Examples:\n  nexa test packages/app --contract snake_api.contract.nexa"
    )]
    Test(TestArgs),

    /// Resolve and write a schema-2 project/package lockfile.
    #[command(
        after_help = "Examples:\n  nexa lock packages/app\n  nexa lock --project nexa.dev.toml"
    )]
    Lock(LockArgs),

    /// Run a source file, package, or selected project package.
    #[command(after_help = "Options must precede PATH. Every value after PATH is script argv.")]
    Run(RunArgs),

    /// Start a persistent transactional REPL.
    #[command(after_help = "Examples:\n  nexa repl\n  nexa repl --history 512 --no-prompt")]
    Repl(ReplArgs),

    /// Execute a verified Bytecode v7 module.
    #[command(after_help = "Examples:\n  nexa exec game.nxb\n  nexa exec game.nxb --trace")]
    Exec(ExecArgs),

    /// Watch and reload a schema-2 project.
    #[command(after_help = "Examples:\n  nexa dev --project nexa.dev.toml")]
    Dev(DevArgs),

    /// Compile one source file to its default .nxb destination.
    #[command(after_help = "Examples:\n  nexa compile game.nexa")]
    Compile { file: PathBuf },

    /// Inspect a Bytecode v7 module.
    #[command(after_help = "Examples:\n  nexa dump game.nxb --section Code")]
    Dump(DumpArgs),

    /// Run the Language Server Protocol endpoint over stdio.
    #[command(after_help = "Example:\n  nexa lsp")]
    Lsp,

    /// Validate a migration between two modules.
    #[command(
        after_help = "Example:\n  nexa migrate check --old-module old.nxb --new-module new.nxb --state state.json"
    )]
    Migrate {
        #[command(subcommand)]
        command: MigrateCommand,
    },

    /// Validate or generate Contract bindings.
    #[command(
        after_help = "Examples:\n  nexa contract check snake_api.contract.nexa\n  nexa contract generate snake_api.contract.nexa"
    )]
    Contract {
        #[command(subcommand)]
        command: ContractCommand,
    },

    /// Internal qualification and repository-audit commands.
    #[command(hide = true)]
    Qa {
        #[command(subcommand)]
        command: QaCommand,
    },
}

#[derive(Clone, Debug, Args)]
pub(crate) struct CheckArgs {
    /// Source file or Package directory.
    #[arg(conflicts_with = "project")]
    pub(crate) input: Option<PathBuf>,

    /// Schema-2 project manifest.
    #[arg(long, conflicts_with_all = ["input", "contract", "policy", "manifest_only", "limits_file"])]
    pub(crate) project: Option<PathBuf>,

    /// Direct Package Host contract.
    #[arg(long, conflicts_with = "manifest_only")]
    pub(crate) contract: Option<PathBuf>,

    /// Direct Package Source Policy.
    #[arg(long, requires = "contract", conflicts_with = "manifest_only")]
    pub(crate) policy: Option<PathBuf>,

    /// Parse and validate only package.toml.
    #[arg(long, conflicts_with_all = ["contract", "policy", "limits_file"])]
    pub(crate) manifest_only: bool,

    /// Verifier limits for a single source input.
    #[arg(long)]
    pub(crate) limits_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct BuildArgs {
    /// Source file or Application Package directory.
    #[arg(conflicts_with = "project")]
    pub(crate) input: Option<PathBuf>,

    /// Schema-2 project manifest.
    #[arg(long, conflicts_with_all = ["input", "contract"])]
    pub(crate) project: Option<PathBuf>,

    /// Direct Package Host contract.
    #[arg(long, conflicts_with = "project")]
    pub(crate) contract: Option<PathBuf>,

    /// Bytecode file or project output directory.
    #[arg(short, long)]
    pub(crate) output: Option<PathBuf>,

    #[arg(long)]
    pub(crate) limits_file: Option<PathBuf>,

    #[arg(long)]
    pub(crate) dump_source_map: bool,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct TestArgs {
    /// Package directory.
    #[arg(conflicts_with = "project")]
    pub(crate) input: Option<PathBuf>,

    /// Schema-2 project manifest.
    #[arg(long, conflicts_with_all = ["input", "contract"])]
    pub(crate) project: Option<PathBuf>,

    /// Direct Package Host contract.
    #[arg(long, conflicts_with = "project")]
    pub(crate) contract: Option<PathBuf>,

    #[arg(long, default_value_t = 1_000_000, value_parser = positive_u64)]
    pub(crate) fuel: u64,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct LockArgs {
    /// Package directory.
    #[arg(conflicts_with = "project")]
    pub(crate) input: Option<PathBuf>,

    /// Schema-2 project manifest.
    #[arg(long, conflicts_with = "input")]
    pub(crate) project: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct RunArgs {
    #[arg(long, requires = "package")]
    pub(crate) project: Option<PathBuf>,

    #[arg(long, requires = "project")]
    pub(crate) package: Option<String>,

    #[arg(long, default_value_t = 20_000, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) fuel: u64,

    #[arg(long)]
    pub(crate) limits_file: Option<PathBuf>,

    /// Path followed by script arguments, or only script arguments with --project.
    #[arg(
        value_name = "PATH_OR_ARG",
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    pub(crate) path_and_args: Vec<OsString>,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct ReplArgs {
    #[arg(long, default_value_t = 20_000, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) fuel: u64,

    #[arg(long, default_value_t = 4_096, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) heap_objects: u32,

    #[arg(long, default_value_t = 1_024, value_parser = positive_usize)]
    pub(crate) max_cells: usize,

    #[arg(long, default_value_t = 256, value_parser = positive_usize)]
    pub(crate) history: usize,

    #[arg(long, default_value_t = 1_048_576, value_parser = positive_usize)]
    pub(crate) max_output_bytes: usize,

    #[arg(long)]
    pub(crate) no_prompt: bool,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct ExecArgs {
    pub(crate) module: PathBuf,

    #[arg(long, default_value_t = 0)]
    pub(crate) function: u32,

    #[arg(long = "arg-i32")]
    pub(crate) arguments: Vec<i32>,

    #[arg(long, default_value_t = 1_000_000, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) fuel: u64,

    #[arg(long)]
    pub(crate) limits_file: Option<PathBuf>,

    #[arg(long)]
    pub(crate) trace: bool,

    #[arg(long, requires = "trace")]
    pub(crate) trace_output: Option<PathBuf>,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct DevArgs {
    #[arg(long)]
    pub(crate) project: PathBuf,

    #[arg(long)]
    pub(crate) once: bool,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct DumpArgs {
    pub(crate) module: PathBuf,

    #[arg(long, conflicts_with = "dump_source_map")]
    pub(crate) section: Option<String>,

    #[arg(long, conflicts_with = "section")]
    pub(crate) dump_source_map: bool,
}

#[derive(Clone, Debug, Subcommand)]
pub(crate) enum MigrateCommand {
    /// Execute the verified migration and validate the resulting snapshot.
    Check(MigrateCheckArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum MigrateFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct MigrateCheckArgs {
    #[arg(long)]
    pub(crate) old_module: PathBuf,
    #[arg(long)]
    pub(crate) new_module: PathBuf,
    #[arg(long)]
    pub(crate) state: PathBuf,
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = MigrateFormat::Human)]
    pub(crate) format: MigrateFormat,
    #[arg(long)]
    pub(crate) dump_state: bool,
    #[arg(long)]
    pub(crate) diff_state: bool,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) max_objects: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) max_fields: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) max_forwarding_entries: Option<u32>,
    #[arg(long, value_parser = positive_usize)]
    pub(crate) max_state_bytes: Option<usize>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) max_gc_roots: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    pub(crate) max_fuel: Option<u64>,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub(crate) max_call_depth: Option<u16>,
}

#[derive(Clone, Debug, Subcommand)]
pub(crate) enum ContractCommand {
    /// Parse and validate a Contract file.
    Check { file: PathBuf },
    /// Generate structured Rust bindings for a Contract file.
    Generate { file: PathBuf },
}

#[derive(Clone, Debug, Subcommand)]
pub(crate) enum QaCommand {
    Models,
    ModelReplay {
        artifact: PathBuf,
    },
    Corpus {
        #[arg(long, value_enum, default_value_t = CorpusFormat::Human)]
        format: CorpusFormat,
    },
    Fixtures {
        input: PathBuf,
    },
    Baseline,
    Machines,
    Verify(VerifyArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CorpusFormat {
    Human,
    Json,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct VerifyArgs {
    pub(crate) input: PathBuf,
    #[arg(long)]
    pub(crate) limits_file: Option<PathBuf>,
}

fn positive_usize(value: &str) -> Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|_| "value must be a positive integer".to_owned())?;
    if value == 0 {
        Err("value must be greater than zero".to_owned())
    } else {
        Ok(value)
    }
}

fn positive_u64(value: &str) -> Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|_| "value must be an unsigned integer".to_owned())?;
    if value == 0 {
        Err("value must be greater than zero".to_owned())
    } else {
        Ok(value)
    }
}

pub(crate) fn diagnostic_format_hint(arguments: &[OsString]) -> DiagnosticFormat {
    arguments
        .windows(2)
        .find_map(|pair| {
            (pair[0] == "--diagnostic-format")
                .then(|| pair[1].to_str())
                .flatten()
        })
        .and_then(|value| DiagnosticFormat::from_str(value, true).ok())
        .unwrap_or(DiagnosticFormat::Human)
}

impl Cli {
    pub(crate) fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        let parsed = <Self as Parser>::try_parse_from(arguments)?;
        parsed.validate_script_candidate()?;
        Ok(parsed)
    }

    fn validate_script_candidate(&self) -> Result<(), clap::Error> {
        let Some(candidate) = self
            .script_and_args
            .first()
            .and_then(|value| value.to_str())
        else {
            return Ok(());
        };
        if REMOVED_TOP_LEVEL_COMMANDS.contains(&candidate) {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::InvalidSubcommand,
                format!("unrecognized subcommand `{candidate}`"),
            )
            .with_cmd(&Self::command()));
        }
        if candidate == "rpel" {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::InvalidSubcommand,
                "unrecognized subcommand `rpel`\n\n  tip: a similar subcommand exists: `repl`",
            )
            .with_cmd(&Self::command()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{Cli, Command};

    #[test]
    fn node_style_script_arguments_begin_at_the_path() {
        let cli = Cli::try_parse_from(["nexa", "--fuel", "9", "game.nexa", "--fuel", "1"])
            .expect("top-level script");
        assert_eq!(cli.fuel, Some(9));
        assert_eq!(
            cli.script_and_args,
            ["game.nexa", "--fuel", "1"].map(OsString::from)
        );

        let cli = Cli::try_parse_from(["nexa", "run", "--fuel", "9", "game.nexa", "--fuel", "1"])
            .expect("run command");
        let Some(Command::Run(run)) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(run.fuel, 9);
        assert_eq!(
            run.path_and_args,
            ["game.nexa", "--fuel", "1"].map(OsString::from)
        );
    }

    #[test]
    fn removed_top_level_commands_are_rejected() {
        assert!(Cli::try_parse_from(["nexa", "trace", "module.nxb"]).is_err());
        assert!(Cli::try_parse_from(["nexa", "migrate-check"]).is_err());
        assert!(Cli::try_parse_from(["nexa", "verify", "module.nxb"]).is_err());
    }
}
