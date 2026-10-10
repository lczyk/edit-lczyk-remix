use std::path::PathBuf;
use std::sync::LazyLock;

use edit::cell::{Ref, SemiRefCell};
use edit::lsh::{LANGUAGES, Language};
use stdext::arena::{read_to_string, scratch_arena};
use stdext::arena_format;

use crate::apperr;

pub struct Settings {
    pub path: PathBuf,
    pub file_associations: Vec<(String, &'static Language)>,
}

struct SettingsCell(SemiRefCell<Settings>);
unsafe impl Sync for SettingsCell {}
static SETTINGS: LazyLock<SettingsCell> =
    LazyLock::new(|| SettingsCell(SemiRefCell::new(Settings::new())));

impl Settings {
    fn new() -> Self {
        Settings { path: PathBuf::new(), file_associations: Vec::new() }
    }

    pub fn borrow() -> Ref<'static, Settings> {
        SETTINGS.0.borrow()
    }

    pub fn reload() -> apperr::Result<()> {
        let s = &mut *SETTINGS.0.borrow_mut();

        // Reset all members if we had been loaded previously.
        if !s.path.as_os_str().is_empty() {
            *s = Settings::new();
        }

        s.load()
    }

    fn load(&mut self) -> apperr::Result<()> {
        self.path = match associations_path() {
            Some(p) => p,
            None => return Ok(()),
        };

        let scratch = scratch_arena(None);
        let str = match read_to_string(&scratch, &self.path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
            Ok(str) => str,
        };
        self.load_str(&str)
    }

    fn load_str(&mut self, str: &str) -> apperr::Result<()> {
        let scratch = scratch_arena(None);
        let Ok(doc) = yaml::parse(str) else {
            return Err(apperr::Error::SettingsInvalid("Invalid YAML"));
        };
        let Some(associations) = doc.get("associations") else {
            return Ok(());
        };
        let Some(associations) = associations.as_map() else {
            return Err(apperr::Error::SettingsInvalid("associations not a mapping"));
        };

        for (k, v) in associations {
            let mut key: &str = k;
            if !key.contains('/') {
                key = arena_format!(&*scratch, "**/{key}").leak();
            }
            let Some(id) = v.as_str() else {
                return Err(apperr::Error::SettingsInvalid("associations value"));
            };
            let Some(language) = LANGUAGES.iter().find(|lang| lang.id == id) else {
                return Err(apperr::Error::SettingsInvalid("language ID"));
            };
            self.file_associations.push((key.to_string(), language));
        }

        Ok(())
    }
}

fn associations_path() -> Option<PathBuf> {
    let mut config_dir = config_dir()?;
    config_dir.push("associations.yaml");
    Some(config_dir)
}

pub fn config_dir() -> Option<PathBuf> {
    fn var_path(key: &str) -> Option<PathBuf> {
        std::env::var_os(key).map(PathBuf::from)
    }

    fn push(mut path: PathBuf, suffix: &str) -> PathBuf {
        path.push(suffix);
        path
    }

    var_path("XDG_CONFIG_HOME")
        .or_else(|| var_path("HOME").map(|p| push(p, ".config")))
        .map(|p| push(p, "edit"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> apperr::Result<Vec<(String, &'static str)>> {
        let mut s = Settings::new();
        s.load_str(text)?;
        Ok(s.file_associations.into_iter().map(|(k, l)| (k, l.id)).collect())
    }

    #[test]
    fn associations_map_globs_to_languages() {
        let got = load("associations:\n  \"*.zzz\": markdown\n  dir/x.conf: markdown\n").unwrap();
        assert_eq!(
            got,
            [("**/*.zzz".to_string(), "markdown"), ("dir/x.conf".to_string(), "markdown")]
        );
    }

    #[test]
    fn a_file_without_associations_is_fine() {
        assert!(load("").unwrap().is_empty());
        assert!(load("other: 1\n").unwrap().is_empty());
    }

    #[test]
    fn bad_associations_are_errors() {
        assert!(load("associations: [a]\n").is_err());
        assert!(load("associations:\n  \"*.zzz\": no-such-language\n").is_err());
        assert!(load("associations:\n  \"*.zzz\": [markdown]\n").is_err());
        assert!(load("associations:\n  *.zzz: markdown\n").is_err());
    }
}
