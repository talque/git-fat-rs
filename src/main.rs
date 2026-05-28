mod backend;
mod fat;

use clap::{Parser, Subcommand};
use std::io;
use std::path::PathBuf;

use log::LevelFilter;

#[derive(Parser)]
#[command(name = "git-fat", about = "Large file support for Git")]
struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Enable debug output
    #[arg(short, long, global = true)]
    debug: bool,

    /// Path to .gitfat config file
    #[arg(short, long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize configuration for git-fat filters
    Init,

    /// Git clean filter: convert a large file to a fat placeholder
    FilterClean {
        /// Filename being filtered (passed by git via %f)
        filename: Option<String>,
    },

    /// Git smudge filter: restore a fat placeholder to the original file
    FilterSmudge {
        /// Filename being filtered (passed by git via %f)
        filename: Option<String>,
    },

    /// Push fat objects to the remote store
    Push {
        /// Backend name to use (default: first in .gitfat)
        backend: Option<String>,
    },

    /// Pull fat objects from the remote store
    Pull {
        /// Backend name to use (default: first in .gitfat), or a file pattern
        backend_or_pattern: Option<String>,
        /// Additional file patterns to pull
        patterns: Vec<String>,
    },

    /// Restore placeholder files in the working tree that have objects in the cache
    Checkout,

    /// Find files in repository history over a size threshold
    Find {
        /// Minimum file size in bytes
        size: usize,
    },

    /// Show orphan (referenced but not cached) and stale (cached but not referenced) objects
    Status,

    /// List all git-fat managed files: digest -> path
    List,

    /// Convert files to git-fat placeholders (for use with git filter-branch --index-filter)
    IndexFilter {
        /// File containing list of paths to convert
        filelist: PathBuf,

        /// Do not update .gitattributes
        #[arg(short = 'x', long = "no-gitattributes")]
        no_gitattributes: bool,
    },
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    let level = if cli.debug {
        LevelFilter::Debug
    } else if cli.verbose {
        LevelFilter::Info
    } else {
        LevelFilter::Warn
    };
    env_logger::Builder::new().filter_level(level).init();

    let gf = fat::GitFat::new(cli.verbose, cli.debug, cli.config.clone())?;

    match &cli.command {
        Command::Init => {
            Ok(())
        }

        Command::Checkout => {
            gf.checkout(false)
        }

        Command::FilterClean { filename } => {
            gf.filter_clean(filename.as_deref(), io::stdin(), io::stdout())
        }

        Command::FilterSmudge { filename } => {
            gf.filter_smudge(filename.as_deref(), io::stdin(), io::stdout())
        }

        Command::Find { size } => {
            gf.find(*size)
        }

        Command::List => {
            gf.list()
        }

        Command::Status => {
            gf.status()
        }

        Command::Push { backend } => {
            gf.push(backend.as_deref())
        }

        Command::Pull { backend_or_pattern, .. } => {
            gf.pull(backend_or_pattern.as_deref())
        }

        Command::IndexFilter { .. } => {
            eprintln!("Command not yet implemented");
            std::process::exit(1);
        }
    }
}
