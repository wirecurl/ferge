//! Shows or removes what direct transfers left on a server.
//!
//! ```sh
//! cargo run -p ferge-bench --bin ferge-direct -- inspect myserver
//! cargo run -p ferge-bench --bin ferge-direct -- purge user@myserver
//! ```
//!
//! `inspect` lists ferge's jobs (running, stopped, ended without ferge, or
//! answering for another server), their staging folders and job keys in
//! `~/.ssh/authorized_keys`; it changes nothing. `purge` stops every job and
//! removes all of it, after asking (unless `--yes`).
//!
//! It logs in like `ssh` would: `~/.ssh/config`, the agent and the default
//! keys, and the host key must be known already. Passwords are not asked
//! for.

#![allow(clippy::print_stdout, reason = "a command-line tool")]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use ferge_core::interaction::NonInteractive;
use ferge_core::remote::sftp::{SftpOptions, SftpRemote, SshConfigSource};
use ferge_core::transfer::direct;

#[derive(Debug, Parser)]
#[command(
    name = "ferge-direct",
    about = "Shows or removes what ferge's direct transfers left on a server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// An SSH configuration file instead of ~/.ssh/config.
    #[arg(short = 'F', long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// The SSH port.
    #[arg(short, long, global = true)]
    port: Option<u16>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List jobs, staging folders and job keys; changes nothing.
    Inspect {
        /// `[user@]host`, or a host from the SSH configuration.
        server: String,
    },
    /// Stop every job and remove all of it.
    Purge {
        /// `[user@]host`, or a host from the SSH configuration.
        server: String,
        /// Do not ask first.
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let server = match &cli.command {
        Command::Inspect { server } | Command::Purge { server, .. } => server.clone(),
    };
    let (user, host) = match server.split_once('@') {
        Some((user, host)) => (Some(user.to_owned()), host.to_owned()),
        None => (None, server.clone()),
    };
    let mut options = SftpOptions::new(&host);
    options.user = user;
    options.port = cli.port;
    if let Some(config) = &cli.config {
        options.ssh_config = SshConfigSource::File(config.clone());
    }
    let remote = SftpRemote::connect(options, Arc::new(NonInteractive))
        .await
        .with_context(|| {
            format!(
                "connecting to {server} (with ssh's keys and agent; the host key must be known)"
            )
        })?;

    let found = direct::inspect(&remote).await?;
    if found.trim().is_empty() {
        println!("Nothing of ferge on {server}.");
        return Ok(());
    }
    println!("On {server}:");
    for line in found.lines() {
        println!("  {}", describe(line));
    }
    if let Command::Purge { yes, .. } = cli.command {
        if !yes {
            print!("Stop these jobs and remove all of it? [y/N] ");
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("Nothing removed.");
                return Ok(());
            }
        }
        let done = direct::purge(&remote).await?;
        for line in done.lines() {
            println!("  {line}");
        }
        let left = direct::inspect(&remote).await?;
        if left.trim().is_empty() {
            println!("Nothing of ferge is left on {server}.");
        } else {
            println!("Still there:");
            for line in left.lines() {
                println!("  {}", describe(line));
            }
        }
    }
    Ok(())
}

/// A line of `inspect` in words.
fn describe(line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["job", id, kind, ..] => {
            let what = match *kind {
                "running" => "copying (the job runs)",
                "stopped" => "stopped without cleaning up",
                "ended" => "ended without ferge; a short result is kept",
                "answering" => "answers another server's job",
                _ => "unknown",
            };
            format!("job {id}: {what}")
        }
        ["stage", id, ..] => {
            let path = line.splitn(3, ' ').nth(2).unwrap_or_default();
            format!("job {id}: staging folder {path}")
        }
        ["key", id, expiry] => format!("job {id}: key in ~/.ssh/authorized_keys until {expiry}"),
        ["lock"] => "a lock folder ~/.ssh/.ferge-lock".to_owned(),
        ["folder", path] => format!("the folder ~/{path}"),
        _ => line.to_owned(),
    }
}
