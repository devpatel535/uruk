//! `uruk` — the command-line entry point.
//!
//! The engine is built in phases, and this binary grows one subcommand per
//! phase as each component is designed: `crawl`, `index`, `query`, `serve`.
//! Right now it answers for itself and nothing else. That is on purpose — the
//! scaffold exists so the workspace, the lint configuration and CI have
//! something real to compile, lint and test before any engine code lands.

use std::process::ExitCode;

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

const TAGLINE: &str = "A search engine that returns links, not answers.";

const USAGE: &str = "\
Usage: uruk <command>

Commands:
  version    Print the version and exit
  help       Print this message and exit

No engine commands yet. See RESEARCH.md for what is being built and in
what order.";

/// What the argument list asked for.
///
/// Parsing is split out from `main` so the dispatch table can be tested
/// without spawning a process.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Print the version string.
    Version,
    /// Print usage. An empty invocation lands here too: asking `uruk`
    /// what it is, is a question, not a mistake.
    Help,
    /// Something we do not recognise, carrying the offending argument.
    Unknown(String),
}

/// Map raw arguments (without the program name) onto a [`Command`].
fn parse(args: &[String]) -> Command {
    match args.first().map(String::as_str) {
        Some("version" | "-V" | "--version") => Command::Version,
        None | Some("help" | "-h" | "--help") => Command::Help,
        Some(other) => Command::Unknown(other.to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match parse(&args) {
        Command::Version => {
            println!("{NAME} {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Help => {
            println!("{NAME} {VERSION} — {TAGLINE}\n\n{USAGE}");
            ExitCode::SUCCESS
        }
        Command::Unknown(arg) => {
            eprintln!("uruk: unknown command '{arg}'\n\n{USAGE}");
            // 2 is the conventional exit code for a usage error.
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, NAME, TAGLINE, USAGE, VERSION, parse};

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_arguments_is_help_not_an_error() {
        assert_eq!(parse(&args(&[])), Command::Help);
    }

    #[test]
    fn version_is_reachable_by_every_spelling() {
        for spelling in ["version", "-V", "--version"] {
            assert_eq!(
                parse(&args(&[spelling])),
                Command::Version,
                "spelling: {spelling}"
            );
        }
    }

    #[test]
    fn help_is_reachable_by_every_spelling() {
        for spelling in ["help", "-h", "--help"] {
            assert_eq!(
                parse(&args(&[spelling])),
                Command::Help,
                "spelling: {spelling}"
            );
        }
    }

    #[test]
    fn unknown_command_is_reported_verbatim() {
        assert_eq!(
            parse(&args(&["crawl"])),
            Command::Unknown("crawl".to_owned())
        );
    }

    #[test]
    fn first_argument_wins() {
        assert_eq!(parse(&args(&["version", "help"])), Command::Version);
    }

    #[test]
    fn identity_strings_are_populated() {
        assert_eq!(NAME, "uruk");
        assert!(!VERSION.is_empty());
        assert!(TAGLINE.contains("links, not answers"));
        assert!(USAGE.contains("Usage: uruk"));
    }
}
