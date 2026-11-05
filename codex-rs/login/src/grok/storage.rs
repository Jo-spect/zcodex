use super::Credentials;
use codex_config::types::AuthCredentialsStoreMode;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use sha2::Digest;
use sha2::Sha256;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::PathBuf;

pub(super) struct Storage {
    pub home: PathBuf,
    pub mode: AuthCredentialsStoreMode,
}

impl Storage {
    pub async fn lock(&self) -> io::Result<File> {
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&home)?;
            let mut options = OpenOptions::new();
            options.create(true).read(true).write(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(home.join("grok-auth.lock"))?;
            file.lock()?;
            Ok(file)
        })
        .await
        .map_err(io::Error::other)?
    }

    fn account(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(self.home.to_string_lossy().as_bytes())
        )
    }

    pub fn load(&self) -> io::Result<Credentials> {
        let json = match self.mode {
            AuthCredentialsStoreMode::Keyring => DefaultKeyringStore
                .load("Codex Grok", &self.account())
                .map_err(io::Error::other)?,
            AuthCredentialsStoreMode::Auto => DefaultKeyringStore
                .load("Codex Grok", &self.account())
                .ok()
                .flatten(),
            AuthCredentialsStoreMode::File => None,
            AuthCredentialsStoreMode::Ephemeral => {
                return Err(io::Error::other(
                    "Grok login requires persistent credential storage",
                ));
            }
        };
        let json = match json {
            Some(json) => json,
            None if self.mode != AuthCredentialsStoreMode::Keyring => {
                std::fs::read_to_string(self.home.join("grok-auth.json")).map_err(|_| {
                    io::Error::other("Grok credentials unavailable; run `codex login grok`")
                })?
            }
            None => {
                return Err(io::Error::other(
                    "Grok credentials unavailable; run `codex login grok`",
                ));
            }
        };
        serde_json::from_str(&json)
            .map_err(|_| io::Error::other("Invalid Grok credentials; run `codex login grok`"))
    }

    pub fn save(&self, credentials: &Credentials) -> io::Result<()> {
        let json = serde_json::to_string(credentials).map_err(io::Error::other)?;
        match self.mode {
            AuthCredentialsStoreMode::Keyring | AuthCredentialsStoreMode::Auto => {
                match DefaultKeyringStore.save("Codex Grok", &self.account(), &json) {
                    Ok(()) => {
                        match std::fs::remove_file(self.home.join("grok-auth.json")) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                        return Ok(());
                    }
                    Err(error) if self.mode == AuthCredentialsStoreMode::Keyring => {
                        return Err(io::Error::other(error));
                    }
                    Err(_) => {}
                }
            }
            AuthCredentialsStoreMode::File => {}
            AuthCredentialsStoreMode::Ephemeral => {
                return Err(io::Error::other(
                    "Grok login requires persistent credential storage",
                ));
            }
        }
        let temporary = self
            .home
            .join(format!(".grok-auth-{}.tmp", rand::random::<u64>()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&temporary)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, self.home.join("grok-auth.json"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}
