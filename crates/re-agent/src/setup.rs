//! Built-in provider profiles and atomic configuration writes. No secrets.
use crate::provider::ProviderConfig;
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OpenAi,
    Anthropic,
    Copilot,
}
impl Kind {
    pub const ALL: [Self; 3] = [Self::OpenAi, Self::Anthropic, Self::Copilot];
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Copilot => "copilot",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Copilot => "GitHub Copilot",
        }
    }
    pub fn model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4.1-mini",
            Self::Anthropic => "claude-sonnet-4-5",
            Self::Copilot => "auto",
        }
    }
    pub fn copilot_script(helper: &Path) -> std::path::PathBuf {
        helper
            .parent()
            .unwrap_or(Path::new("."))
            .join("copilot-sdk/dist/main.js")
    }
    pub fn config(self, model: &str, helper: &Path) -> Result<ProviderConfig, String> {
        let model = model.trim();
        if ["sk-", "ghp_", "ghu_", "github_pat_"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            return Err("This looks like a credential. Use the API key field or GitHub sign-in, not the model field.".into());
        }
        if model.is_empty() || model.len() > 200 || model.contains(['\r', '\n']) {
            return Err("Enter a model name".into());
        }
        let mut command = vec![
            helper.to_string_lossy().into_owned(),
            "--backend".into(),
            self.id().into(),
        ];
        if self != Self::Copilot {
            command.extend(["--credential".into(), self.id().into()]);
        } else {
            command.extend([
                "--copilot-sdk".into(),
                Self::copilot_script(helper).to_string_lossy().into_owned(),
            ]);
        }
        if self == Self::OpenAi {
            // Works for both standard and reasoning models on OpenAI's API.
            command.extend(["--max-tokens-field".into(), "max_completion_tokens".into()]);
        }
        Ok(ProviderConfig {
            id: self.id().into(),
            name: self.name().into(),
            model: model.into(),
            command,
            destination: match self {
                Self::OpenAi => "api.openai.com (OpenAI)",
                Self::Anthropic => "api.anthropic.com (Anthropic)",
                Self::Copilot => "GitHub Copilot (official SDK)",
            }
            .into(),
            dir: Default::default(),
        })
    }
}

pub fn save(root: &Path, config: &ProviderConfig) -> io::Result<()> {
    if !Kind::ALL.iter().any(|k| k.id() == config.id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a built-in provider",
        ));
    }
    let folder = root.join(&config.id);
    std::fs::create_dir_all(&folder)?;
    let path = folder.join("provider.json");
    let temp = folder.join(format!(".provider-{}.tmp", crate::conversation::new_id()));
    let result = (|| {
        use io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(config).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &path)?;
        set_default(root, &config.id)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn set_default(root: &Path, id: &str) -> io::Result<()> {
    use io::Write;
    let temp = root.join(format!(".default-{}.tmp", crate::conversation::new_id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(id.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, root.join("default-provider.txt"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub fn default_id(root: &Path) -> Option<String> {
    std::fs::read_to_string(root.join("default-provider.txt")).ok()
}

pub fn remove(root: &Path, kind: Kind) -> io::Result<()> {
    match std::fs::remove_file(root.join(kind.id()).join("provider.json")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_round_trip_and_do_not_contain_keys() {
        let root = std::env::temp_dir().join(format!(
            "re-provider-setup-{}",
            crate::conversation::new_id()
        ));
        for kind in Kind::ALL {
            let mut c = kind
                .config(kind.model(), Path::new("/tools/rusteditor-openai"))
                .unwrap();
            save(&root, &c).unwrap();
            c.model = "updated".into();
            save(&root, &c).unwrap();
        }
        let (loaded, errors) = crate::provider::load_configs(&root);
        assert!(errors.is_empty());
        assert_eq!(loaded.len(), 3);
        assert_eq!(default_id(&root).as_deref(), Some("copilot"));
        for c in loaded {
            assert_eq!(c.model, "updated");
            assert!(!serde_json::to_string(&c).unwrap().contains("sk-secret"));
        }
        let copilot = Kind::Copilot
            .config("auto", Path::new("/tools/rusteditor-openai"))
            .unwrap();
        assert!(copilot.command.contains(&"--copilot-sdk".into()));
        assert!(
            copilot
                .command
                .last()
                .unwrap()
                .ends_with("copilot-sdk/dist/main.js")
                || copilot
                    .command
                    .last()
                    .unwrap()
                    .ends_with("copilot-sdk\\dist\\main.js")
        );
        assert!(Kind::OpenAi.config("", Path::new("x")).is_err());
        assert!(Kind::OpenAi.config("sk-secret", Path::new("x")).is_err());
        remove(&root, Kind::Anthropic).unwrap();
        assert_eq!(crate::provider::load_configs(&root).0.len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}
