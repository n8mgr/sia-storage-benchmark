//! The stored app key and the approval flow that produces it.

use std::fs;
use std::io::{BufRead, IsTerminal, stdin};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use sia_storage::{AppKey, AppMetadata, Builder, Sdk, generate_recovery_phrase};

pub const DEFAULT_INDEXER: &str = "https://sia.storage";

/// Changing the ID invalidates every existing app key.
const APP_METADATA: AppMetadata = AppMetadata {
    id: sia_storage::app_id!("444329386e78de4617abf118d7bcb97e4e3056de678049a7911ad68262107427"),
    name: "blog-benchmark",
    description: "Upload and download benchmark",
    service_url: "https://github.com/SiaFoundation/blog-benchmark",
    logo_url: None,
    callback_url: None,
};

#[derive(Deserialize, Serialize)]
struct Config {
    indexer: String,
    /// 32-byte app key, hex-encoded.
    app_key: String,
}

fn path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("tech", "Sia", "blog-benchmark")
        .ok_or_else(|| anyhow!("could not determine the config directory"))?;
    Ok(dirs.config_dir().join("config.toml"))
}

/// Reads a recovery phrase without echoing it when stdin is a terminal. The
/// phrase derives the app key and is never stored.
fn read_recovery_phrase() -> Result<String> {
    if stdin().is_terminal() {
        return rpassword::prompt_password("Recovery phrase: ")
            .context("failed to read the recovery phrase");
    }
    stdin()
        .lock()
        .lines()
        .next()
        .ok_or_else(|| anyhow!("no recovery phrase on stdin"))?
        .context("failed to read the recovery phrase")
}

/// Runs the indexer's approval flow and stores the app key. The key derives
/// from the recovery phrase, this tool's app ID, and the approving account, so
/// the same phrase and account always produce the same key.
pub async fn login(indexer: &str, new: bool) -> Result<()> {
    let builder = Builder::new(indexer, APP_METADATA).context("invalid indexer URL")?;
    let builder = builder
        .request_connection()
        .await
        .context("failed to request a connection")?;
    println!("Authorize the application at: {}", builder.response_url());
    let builder = builder
        .wait_for_approval()
        .await
        .context("failed waiting for approval")?;
    println!("Connection approved.");

    let phrase = if new {
        let phrase = generate_recovery_phrase();
        println!("Generated recovery phrase (write it down):\n  {phrase}");
        phrase
    } else {
        read_recovery_phrase()?
    };
    let phrase = phrase.trim();
    sia_storage::validate_recovery_phrase(phrase).context("invalid recovery phrase")?;
    let sdk = builder
        .register(phrase)
        .await
        .context("failed to register the app key")?;

    let path = path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let config = Config {
        indexer: indexer.trim().to_string(),
        app_key: hex::encode(sdk.app_key().export()),
    };
    fs::write(&path, toml::to_string_pretty(&config)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    println!("Saved to {} (indexer: {indexer})", path.display());
    Ok(())
}

/// Connects to the stored indexer with the stored app key.
pub async fn connect() -> Result<Sdk> {
    let path = path()?;
    let text = fs::read_to_string(&path)
        .map_err(|e| anyhow!("{e}; run `blog-benchmark login` first"))
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;

    let key = hex::decode(config.app_key.trim()).context("invalid app key")?;
    let seed: [u8; 32] = key
        .get(..32)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| anyhow!("invalid app key: expected at least 32 bytes"))?;

    let builder = Builder::new(&config.indexer, APP_METADATA).context("invalid indexer URL")?;
    let sdk = builder
        .connected(&AppKey::import(seed))
        .await
        .context("failed to connect to the indexer")?
        .ok_or_else(|| {
            anyhow!(
                "the app key is not authorized by {}; run `blog-benchmark login`",
                config.indexer
            )
        })?;
    if !sdk
        .account()
        .await
        .context("failed to read the account")?
        .ready
    {
        bail!("the account is not ready; the indexer is still propagating registration");
    }
    Ok(sdk)
}
