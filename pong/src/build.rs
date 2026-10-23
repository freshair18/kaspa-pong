//! Compile this project's covenant through Argent's public SDK.
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Build and inspect Pong covenants using the pinned Argent SDK")]
struct Options {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile into a scratch directory, leaving installed game artifacts alone.
    Build {
        #[arg(default_value = "contracts/pong.ag")]
        source: PathBuf,
        #[arg(long, default_value = "target/argent/pong")]
        out: PathBuf,
    },
    /// Report actor bytecode, state, opcode and witness sizes without recompiling.
    Inspect {
        #[arg(default_value = "artifacts")]
        input: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse();
    let (source, out) = match options.command {
        Command::Inspect { input } => {
            let report = argent::inspect::inspect_path(input)?;
            print!("{}", argent::inspect::render_report(&report));
            return Ok(());
        }
        Command::Build { source, out } => (source, out),
    };
    let artifact = argent::build_file(&source, &out)?;
    println!("{}", serde_json::json!({"artifact": out.join("artifact.json"), "id": artifact.id}));
    Ok(())
}

#[cfg(test)]
mod cli {
    use super::*;

    #[test]
    fn builds_require_an_explicit_command_and_default_to_scratch() {
        let options = Options::try_parse_from(["pong-build", "build"]).unwrap();
        let Command::Build { source, out } = options.command else { panic!() };
        assert_eq!(source, PathBuf::from("contracts/pong.ag"));
        assert_eq!(out, PathBuf::from("target/argent/pong"));
        assert!(Options::try_parse_from(["pong-build"]).is_err());
    }

    #[test]
    fn inspect_accepts_a_directory_or_artifact_without_building() {
        for input in ["artifacts", "artifacts/artifact.json"] {
            let options = Options::try_parse_from(["pong-build", "inspect", input]).unwrap();
            let Command::Inspect { input: actual } = options.command else { panic!() };
            assert_eq!(actual, PathBuf::from(input));
        }
    }
}
