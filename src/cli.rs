use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "llm-to-rss",
    version,
    about = "Turn scheduled LLM prompts into virtual Atom/RSS feeds."
)]
pub struct Cli {
    /// Path to configuration file.
    #[arg(
        short,
        long,
        default_value = "config.yaml",
        global = true,
        env = "LLM_TO_RSS_CONFIG"
    )]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the HTTP feed server and background cron scheduler.
    Serve,

    /// Run one or all enabled feeds immediately.
    Run {
        /// Run only the feed with this ID.
        #[arg(short, long)]
        feed: Option<String>,

        /// Execute the LLM call and print output without writing to SQLite.
        #[arg(long)]
        dry_run: bool,
    },

    /// Validate configuration and all feed YAML files.
    Validate,

    /// List configured virtual feeds, schedules, and stored item counts.
    List,

    /// Print an OPML 2.0 subscription file for RSS reader import.
    Opml {
        /// Override base URL used in feed xmlUrl attributes.
        #[arg(long)]
        base_url: Option<String>,
    },
}
