//! cowt — the co-worktree command-line tool.

mod backend;
mod cli;
mod commands;
mod state;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};

fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cowt: error: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn dispatch(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Fork {
            path,
            name,
            force_path,
        } => {
            commands::fork::fork(commands::fork::ForkArgs {
                path,
                name,
                force_path,
            })?;
            Ok(0)
        }
        Cmd::Run { id, cmd } => commands::run::run(commands::run::RunArgs { id, cmd }),
        Cmd::Diff {
            id,
            json,
            content,
            stat,
        } => {
            commands::diff::diff_cmd(commands::diff::DiffArgs {
                id,
                json,
                content,
                stat,
            })?;
            Ok(0)
        }
        Cmd::Apply { id, dry_run, json } => {
            commands::apply::apply(commands::apply::ApplyArgs { id, dry_run, json })
        }
        Cmd::Drop { id, force } => {
            commands::drop::drop_cmd(commands::drop::DropArgs { id, force })?;
            Ok(0)
        }
        Cmd::List { json } => {
            commands::list::list(json)?;
            Ok(0)
        }
        Cmd::Status { id, json } => {
            commands::list::status(&id, json)?;
            Ok(0)
        }
        Cmd::Doctor => {
            commands::list::doctor()?;
            Ok(0)
        }
    }
}
