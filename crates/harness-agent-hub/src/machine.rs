use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[must_use]
pub fn gate_dir(home: &Path) -> PathBuf {
    home.join("machine")
}

#[must_use]
pub fn gate_name(session: &str) -> String {
    session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[must_use]
pub fn gate_path(home: &Path, session: &str) -> PathBuf {
    gate_dir(home).join(gate_name(session))
}

pub(crate) struct Machine {
    home: PathBuf,
    on: Mutex<HashSet<String>>,
}

impl Machine {
    pub(crate) async fn load(home: &Path) -> Self {
        let mut on = HashSet::new();
        if let Ok(mut dir) = tokio::fs::read_dir(gate_dir(home)).await {
            while let Ok(Some(entry)) = dir.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    on.insert(name.to_owned());
                }
            }
        }
        Self {
            home: home.to_path_buf(),
            on: Mutex::new(on),
        }
    }

    pub(crate) fn is_on(&self, session: &str) -> bool {
        let name = gate_name(session);
        self.on.lock().is_ok_and(|set| set.contains(&name))
    }

    pub(crate) async fn set(&self, session: &str, on: bool) -> std::io::Result<()> {
        let path = gate_path(&self.home, session);
        if on {
            tokio::fs::create_dir_all(gate_dir(&self.home)).await?;
            tokio::fs::write(&path, b"on").await?;
        } else {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if let Ok(mut set) = self.on.lock() {
            let name = gate_name(session);
            if on {
                set.insert(name);
            } else {
                set.remove(&name);
            }
        }
        Ok(())
    }
}
