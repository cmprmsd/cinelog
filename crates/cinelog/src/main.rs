mod cli;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use cli::{Cli, Command};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Rec(args) if args.protocol => {
            println!("{}", cinelog_proto::PROTOCOL_VERSION);
            ExitCode::SUCCESS
        }
        Command::Rec(args) if !args.check => {
            let mut it = args.command.into_iter();
            let shell = it.next().map(PathBuf::from).unwrap_or_else(default_shell);
            cinelog_rec::run(cinelog_rec::RecRequest {
                shell,
                args: it.collect(),
            })
        }
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "cinelog", &mut std::io::stdout());
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("cinelog: {other:?} is not available in this build yet");
            ExitCode::from(2)
        }
    }
}

fn default_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}
