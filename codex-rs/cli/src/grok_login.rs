//! Grok account login and the existing command-backed provider auth integration.

use codex_core::config::ConfigBuilder;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_login::grok::GrokLogin;
use codex_utils_cli::CliConfigOverrides;
use std::path::PathBuf;
use toml_edit::value;

pub async fn login(overrides: CliConfigOverrides, proxy: Option<String>) -> anyhow::Result<()> {
    let config = crate::login::load_config_or_exit(overrides).await;
    let proxy = proxy.or_else(|| {
        config
            .model_providers
            .get("grok")
            .and_then(|provider| provider.proxy.clone())
    });
    let login = GrokLogin::new(
        config.codex_home.to_path_buf(),
        config.cli_auth_credentials_store_mode,
        proxy.as_deref(),
    )?;
    let device = login.begin().await?;
    eprintln!("{device}");
    tokio::select! {
        result = login.complete(device) => result?,
        result = tokio::signal::ctrl_c() => {
            result?;
            anyhow::bail!("Grok login cancelled");
        }
    }
    let executable = std::env::current_exe()?.to_string_lossy().into_owned();
    let store = serde_json::to_value(config.cli_auth_credentials_store_mode)?;
    let store = store
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid credential store mode"))?;
    let args = [
        "login".to_string(),
        "grok-token".to_string(),
        "--home".to_string(),
        config.codex_home.to_string_lossy().into_owned(),
        "--store".to_string(),
        store.to_string(),
    ]
    .into_iter()
    .collect::<toml_edit::Array>();
    let mut edits = vec![
        (vec!["name"], value("Grok")),
        (vec!["base_url"], value("https://api.x.ai/v1")),
        (vec!["wire_api"], value("responses")),
        (vec!["requires_openai_auth"], value(/*v*/ false)),
        (vec!["supports_websockets"], value(/*v*/ false)),
        (vec!["auth", "command"], value(executable)),
        (vec!["auth", "args"], value(args)),
        (
            vec!["auth", "cwd"],
            value(config.codex_home.to_string_lossy().into_owned()),
        ),
        (vec!["auth", "timeout_ms"], value(/*v*/ 60_000)),
        (vec!["auth", "refresh_interval_ms"], value(/*v*/ 30_000)),
    ];
    if let Some(proxy) = proxy {
        edits.push((vec!["proxy"], value(proxy)));
    }
    let mut edits: Vec<ConfigEdit> = edits
        .into_iter()
        .map(|(path, value)| ConfigEdit::SetPath {
            segments: ["model_providers", "grok"]
                .into_iter()
                .chain(path)
                .map(str::to_owned)
                .collect(),
            value,
        })
        .collect();
    // Explicit command-backed auth must not be shadowed by an older API-key configuration.
    for key in ["env_key", "experimental_bearer_token", "aws"] {
        edits.push(ConfigEdit::ClearPath {
            segments: vec!["model_providers".into(), "grok".into(), key.into()],
        });
    }
    ConfigEditsBuilder::new(config.codex_home.as_path())
        .with_edits(edits)
        .apply()
        .await?;
    eprintln!("Grok account saved. Start with: codex -c 'model_provider=\"grok\"' -m grok-4.7");
    Ok(())
}

/// Prints only a bearer token for the provider auth subprocess contract.
pub async fn print_token(home: PathBuf, store: String) -> anyhow::Result<()> {
    let config = ConfigBuilder::default().codex_home(home).build().await?;
    let proxy = config
        .model_providers
        .get("grok")
        .and_then(|provider| provider.proxy.as_deref());
    let store = serde_json::from_value(serde_json::Value::String(store))?;
    let login = GrokLogin::new(config.codex_home.to_path_buf(), store, proxy)?;
    println!("{}", login.access_token().await?);
    Ok(())
}
