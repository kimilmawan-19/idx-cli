mod cli;
mod config;
mod output;

use idx_core::runtime;

use clap::CommandFactory;
use clap::Parser;
use clap_complete::{generate, shells};

use crate::cli::{Cli, Commands};
use crate::config::IdxConfig;
use crate::output::{OutputFormat, OutputSetting, emit_error};
use idx_core::error::IdxError;

fn main() {
    restore_sigpipe();
    if let Err(err) = run() {
        std::process::exit(err.exit_code());
    }
}

/// Rust ignores SIGPIPE, so writing to a closed pipe (`idx ... | head -1`)
/// panicked with exit 101. Restore the default: end quietly like other
/// command-line tools.
fn restore_sigpipe() {
    #[cfg(unix)]
    // SAFETY: called first in main, before any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn run() -> Result<(), IdxError> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return Err(usage_error(err)),
    };
    runtime::set_quiet(cli.quiet);
    let config = match IdxConfig::load_with_cli(&cli) {
        Ok(config) => config,
        Err(err) => {
            emit_error(&err, &bootstrap_output_format(&cli));
            return Err(err);
        }
    };
    output::json::configure(config.output, config::stdout_is_terminal());
    output::style::set_color(!config.no_color);

    match &cli.command {
        Commands::Version => {
            if config.output.is_json() {
                output::json::print_json(&serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION")
                }))?;
            } else {
                println!("{}", env!("CARGO_PKG_VERSION"));
            }
        }
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_owned();
            match shell {
                cli::Shell::Bash => generate(shells::Bash, &mut cmd, name, &mut std::io::stdout()),
                cli::Shell::Zsh => generate(shells::Zsh, &mut cmd, name, &mut std::io::stdout()),
                cli::Shell::Fish => generate(shells::Fish, &mut cmd, name, &mut std::io::stdout()),
            }
        }
        Commands::Stocks(stocks) => {
            if let Err(err) = cli::stocks::handle(stocks, &config, cli.offline, cli.no_cache) {
                emit_error(&err, &config.output);
                return Err(err);
            }
        }
        Commands::Config(cfg) => {
            if let Err(err) = cli::config::handle(cfg) {
                emit_error(&err, &config.output);
                return Err(err);
            }
        }
        Commands::Cache(cache) => {
            if let Err(err) = cli::cache::handle(cache, &config.output) {
                emit_error(&err, &config.output);
                return Err(err);
            }
        }
        #[cfg(feature = "ownership")]
        Commands::Ownership(cmd) => {
            if let Err(err) = cli::ownership::handle(&cmd.command, &config) {
                emit_error(&err, &config.output);
                return Err(err);
            }
        }
    }

    Ok(())
}

/// Help and version go to stdout as clap renders them. A usage error is a
/// JSON error document in JSON mode (the output contract holds for typos
/// too) and clap's own message otherwise.
fn usage_error(err: clap::Error) -> IdxError {
    use clap::error::ErrorKind;
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) || !argv_output_format().is_json()
    {
        err.exit();
    }
    let rendered = err.render().to_string();
    let message = rendered
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .trim()
        .trim_start_matches("error: ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let err = IdxError::InvalidInput(message);
    emit_error(&err, &argv_output_format());
    err
}

/// `-o`/`--output` read straight from argv (clap could not parse it), then
/// `IDX_OUTPUT`.
fn argv_output_format() -> OutputFormat {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let value = if arg == "-o" || arg == "--output" {
            iter.next().cloned()
        } else if let Some(value) = arg.strip_prefix("--output=") {
            Some(value.to_string())
        } else if let Some(value) = arg.strip_prefix("-o") {
            Some(value.trim_start_matches('=').to_string())
        } else {
            None
        };
        if let Some(setting) = value.as_deref().and_then(OutputSetting::parse) {
            flag = Some(setting);
        }
    }
    flag.or_else(|| {
        std::env::var("IDX_OUTPUT")
            .ok()
            .and_then(|value| OutputSetting::parse(&value))
    })
    .unwrap_or_default()
    .resolve(config::stdout_is_terminal())
}

/// Output format for errors raised before the configuration has loaded.
fn bootstrap_output_format(cli: &Cli) -> OutputFormat {
    let setting = cli
        .output
        .or_else(|| {
            std::env::var("IDX_OUTPUT")
                .ok()
                .and_then(|value| OutputSetting::parse(&value))
        })
        .unwrap_or_default();
    setting.resolve(config::stdout_is_terminal())
}
