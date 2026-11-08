mod check;
mod cli;
mod ctx;
mod doctor;
mod install;
mod link;
mod module;
mod pause;
mod search;
mod serve;
mod session;

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
        Command::Rec(args) if args.check => check::run(),
        Command::Rec(args) => {
            let mut it = args.command.into_iter();
            let shell = it.next().map(PathBuf::from).unwrap_or_else(default_shell);
            cinelog_rec::run(cinelog_rec::RecRequest {
                shell,
                args: it.collect(),
            })
        }
        Command::Link(args) => link::run(&args),
        Command::Pause(args) => pause::run(pause::Action::Pause, args.reason),
        Command::Resume => pause::run(pause::Action::Resume, None),
        Command::Doctor(args) => doctor::run(args.json),
        Command::Install(args) => install::run(&args),
        Command::Serve(args) => serve::run(&args),
        Command::Compress(args) => serve::compress(&args),
        Command::Meta(args) => serve::meta(&args),
        Command::Search(args) => search::run(&args),
        Command::Show(args) => search::show(&args),
        Command::Forget(args) => search::forget(&args),
        Command::Index(args) => search::index(&args),
        Command::Status(args) => search::status(&args),
        Command::Import(args) => search::import_report(&args),
        Command::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "cinelog", &mut std::io::stdout());
            ExitCode::SUCCESS
        }
    }
}

fn default_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"))
}
