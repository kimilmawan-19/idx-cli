use clap::{Args, Subcommand};

use serde_json::json;

use crate::config::{config_path, ensure_default_config, get_config_value, set_config_value};
use crate::output::{OutputFormat, json::print_json};
use idx_core::error::IdxError;

#[derive(Debug, Args)]
#[command(about = "Manage configuration")]
pub struct ConfigCmd {
    #[command(subcommand)]
    pub command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigSubcommand {
    #[command(about = "Create default configuration file")]
    Init,
    #[command(about = "Get a configuration value by key")]
    Get { key: String },
    #[command(about = "Set a configuration value by key")]
    Set { key: String, value: String },
    #[command(about = "Print configuration file path")]
    Path,
}

/// Plain text for a terminal or `-o table`; one JSON document in JSON
/// modes, like every other command.
pub fn handle(cmd: &ConfigCmd, format: &OutputFormat) -> Result<(), IdxError> {
    let as_json = format.is_json();
    match &cmd.command {
        ConfigSubcommand::Init => {
            let path = ensure_default_config()?;
            if as_json {
                print_json(&json!({ "path": path.display().to_string() }))?;
            } else {
                println!("{}", path.display());
            }
        }
        ConfigSubcommand::Get { key } => {
            let value = get_config_value(key)?
                .ok_or_else(|| IdxError::ConfigError(format!("key not found: {key}")))?;
            if as_json {
                print_json(&json!({ "key": key, "value": value }))?;
            } else {
                println!("{value}");
            }
        }
        ConfigSubcommand::Set { key, value } => {
            set_config_value(key, value)?;
            if as_json {
                print_json(&json!({ "ok": true, "key": key, "value": value }))?;
            } else {
                println!("ok");
            }
        }
        ConfigSubcommand::Path => {
            let path = config_path()?;
            if as_json {
                print_json(&json!({ "path": path.display().to_string() }))?;
            } else {
                println!("{}", path.display());
            }
        }
    }
    Ok(())
}
