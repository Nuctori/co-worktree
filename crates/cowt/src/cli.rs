//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cowt",
    version,
    about = "co-worktree: Git-worktree-style isolation, review and merge for application config/data directories",
    long_about = "Fork any application's config/data directory into an isolated worktree, run the \
                  application with its writes redirected, review the diff, then apply (three-way \
                  merge) or drop. Not a container, not a VM, not a sandbox."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Create an isolated worktree over a host directory (metadata snapshot only).
    Fork {
        /// Host directory to isolate (must be under $HOME unless --force-path).
        path: PathBuf,
        /// Human-friendly name (auto-derived from the path if omitted).
        #[arg(long)]
        name: Option<String>,
        /// Allow isolating directories outside $HOME (expert escape hatch).
        #[arg(long)]
        force_path: bool,
    },

    /// Run a command in the merged virtual view; writes are redirected to the isolated layer.
    Run {
        /// Worktree id or name.
        id: String,
        /// Command and arguments to execute.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },

    /// Show changes of the isolated layer relative to the fork snapshot.
    Diff {
        /// Worktree id or name.
        id: String,
        /// Output machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Include content details: Myers line diff for text, key-level diff for JSON/YAML.
        #[arg(long)]
        content: bool,
        /// Only print the summary line.
        #[arg(long)]
        stat: bool,
    },

    /// Three-way merge (base / current / worktree) into the host directory.
    Apply {
        /// Worktree id or name.
        id: String,
        /// Preview operations and conflicts without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },

    /// Discard a worktree: unmount and delete all isolated data. Host stays untouched.
    Drop {
        /// Worktree id or name.
        id: String,
        /// Kill a running process and unmount if necessary.
        #[arg(long)]
        force: bool,
    },

    /// List all worktrees.
    List {
        #[arg(long)]
        json: bool,
    },

    /// Show one worktree's details.
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },

    /// Check backend availability on this host.
    Doctor,
}
