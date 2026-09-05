use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum TitleMode {
    Never,
    #[default]
    Prefix,
    Replace,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    Check {
        fixture: PathBuf,
        #[arg(long)]
        agent: Option<String>,
    },
    Agents,
    /// Observe an existing local fux pane without owning its command.
    Observe {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        pane: u32,
        #[arg(long)]
        pid: u32,
    },
}

#[derive(Debug, Parser)]
#[command(version, about, trailing_var_arg = true)]
pub struct Cli {
    #[arg(long)]
    pub events: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = TitleMode::Prefix)]
    pub title: TitleMode,
    #[arg(long)]
    pub no_osc: bool,
    #[arg(long)]
    pub rules: Vec<PathBuf>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub debug: bool,
    #[command(subcommand)]
    pub action: Option<Action>,
    #[arg(allow_hyphen_values = true)]
    pub command: Vec<String>,
}
