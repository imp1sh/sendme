//! Command line entry point for sendme.

use clap::{
    error::{ContextKind, ErrorKind},
    CommandFactory, Parser,
};
use console::style;
use sendme::{receive, send, Args, Commands};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(cause) => {
            if let Some(text) = cause.get(ContextKind::InvalidSubcommand) {
                eprintln!("{} \"{}\"\n", ErrorKind::InvalidSubcommand, text);
                eprintln!("Available subcommands are");
                for cmd in Args::command().get_subcommands() {
                    eprintln!("    {}", style(cmd.get_name()).bold());
                }
                std::process::exit(1);
            } else {
                cause.exit();
            }
        }
    };
    let res = match args.command {
        Commands::Send(args) => send(args).await,
        Commands::Receive(args) => receive(args).await,
    };
    if let Err(e) = &res {
        eprintln!("{e}");
    }
    match res {
        Ok(()) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}
