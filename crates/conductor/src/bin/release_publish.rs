//! CI-only Stable/Nightly publication writer. Not an operator conductor command.
use anyhow::{Context as _, Result, bail};
use jmodguard_conductor::release;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;

fn flag(args: &[String], name: &str) -> Result<String> {
    let mut values = args.iter().skip_while(|arg| arg.as_str() != name).skip(1);
    values
        .next()
        .cloned()
        .context(format!("{name} requires a value"))
}

fn optional_flag(args: &[String], name: &str) -> Option<String> {
    flag(args, name).ok()
}

fn main() {
    if let Err(error) = run() {
        eprintln!("release publication failed: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("frontend-inventory") => {
            let root = PathBuf::from(
                args.get(2)
                    .context("frontend-inventory requires a directory")?,
            );
            let inventory = release::frontend_inventory(&root)?;
            println!("{}", serde_json::to_string(&inventory)?);
        }
        Some("finalize") => {
            let channel = flag(&args, "--channel")?;
            let release_id = flag(&args, "--release-id")?;
            let sha = flag(&args, "--source-commit")?;
            let published_at = flag(&args, "--published-at")?;
            let components_file = PathBuf::from(flag(&args, "--components-file")?);
            let key_id = flag(&args, "--key-id")?;
            let out = PathBuf::from(flag(&args, "--out")?);
            let notes = optional_flag(&args, "--notes").unwrap_or_default();
            let secret = env::var("JMODGUARD_RELEASE_SIGNING_KEY")
                .context("complete manifests require JMODGUARD_RELEASE_SIGNING_KEY")?;
            let components: Value = serde_json::from_str(
                &fs::read_to_string(&components_file).context("components JSON")?,
            )?;
            let path = release::write_complete_publication(release::CompletePublication {
                channel,
                release_id,
                source_commit: sha,
                published_at,
                components,
                notes,
                key_id,
                secret_hex: secret,
                out_dir: out,
            })?;
            println!("{}", path.display());
        }
        Some("-h" | "--help" | "help") | None => {
            print!(
                "jmodguard-release-publish frontend-inventory DIR\n\
                 jmodguard-release-publish finalize --channel stable|nightly --release-id ID \\\n\
                   --source-commit SHA --published-at TIME --components-file FILE \\\n\
                   --key-id ID --out DIR [--notes TEXT]\n\
                 Signing key is JMODGUARD_RELEASE_SIGNING_KEY; never pass it on argv.\n"
            );
        }
        Some(other) => bail!("unknown release publication command {other}"),
    }
    Ok(())
}
