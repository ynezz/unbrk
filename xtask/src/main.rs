use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use clap_complete::{Shell, generate_to};
use clap_mangen::Man;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

fn main() -> Result<()> {
    Xtask::parse().run()
}

#[derive(Debug, Parser)]
#[command(name = "xtask")]
struct Xtask {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Completions {
        #[arg(long, default_value = "target/completions")]
        out_dir: PathBuf,
    },
    Man {
        #[arg(long, default_value = "target/man")]
        out_dir: PathBuf,
    },
}

impl Xtask {
    fn run(self) -> Result<()> {
        match self.command {
            Commands::Completions { out_dir } => generate_completions(&out_dir),
            Commands::Man { out_dir } => generate_manpage(&out_dir),
        }
    }
}

fn generate_completions(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| {
        format!(
            "failed to create completions output directory {}",
            out_dir.display()
        )
    })?;

    for shell in [
        Shell::Bash,
        Shell::Elvish,
        Shell::Fish,
        Shell::PowerShell,
        Shell::Zsh,
    ] {
        let mut command = unbrk_cli::cli_command();
        let path = generate_to(shell, &mut command, "unbrk", out_dir).with_context(|| {
            format!(
                "failed to generate {} completions into {}",
                shell,
                out_dir.display()
            )
        })?;
        println!("{}", path.display());
    }

    Ok(())
}

fn generate_manpage(out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| {
        format!(
            "failed to create man output directory {}",
            out_dir.display()
        )
    })?;
    let path = out_dir.join("unbrk.1");
    let mut file = File::create(&path)
        .with_context(|| format!("failed to create manpage {}", path.display()))?;
    Man::new(unbrk_cli::cli_command())
        .render(&mut file)
        .with_context(|| format!("failed to render manpage {}", path.display()))?;
    println!("{}", path.display());
    Ok(())
}
