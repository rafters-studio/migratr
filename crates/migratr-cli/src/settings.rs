use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use migratr::{AtomicError, Executor, ForeignKeyDef, LedgerRow, SchemaSnapshot};
use serde_json::Value;

use crate::error::CliError;

/// The settings file migratr looks for, walking up from the working directory.
pub const FILE_NAME: &str = "migratr.jsonc";

/// The note a loud run ends with.
pub const QUIET_NOTE: &str = "loud can be turned off in migratr.jsonc: \"loud\": false";

/// The settings a run uses: each key's value, and which keys the file set.
pub struct Settings {
    /// The settings file used, or `None` when none was found.
    pub file: Option<PathBuf>,
    pub smuggl: Option<String>,
    pub loud: bool,
    pub safe: bool,
    pub autoclean: bool,
    /// The keys the file set; every other key holds its default.
    set: Vec<String>,
}

impl Settings {
    fn defaults(file: Option<PathBuf>) -> Self {
        Self {
            file,
            smuggl: None,
            loud: true,
            safe: true,
            autoclean: true,
            set: Vec::new(),
        }
    }

    /// The lines a loud run starts with: the file used, then each key with its value and
    /// whether the file or the default set it.
    pub fn describe(&self) -> Vec<String> {
        let source = |key: &str| {
            if self.set.iter().any(|k| k == key) {
                "set by the file"
            } else {
                "set by the default"
            }
        };
        let file = self.file.as_ref().map_or_else(
            || format!("settings: no {FILE_NAME} found"),
            |path| format!("settings: {}", path.display()),
        );
        let smuggl = self.smuggl.as_deref().unwrap_or("unset");
        vec![
            file,
            format!("smuggl = {smuggl} ({})", source("smuggl")),
            format!("loud = {} ({})", self.loud, source("loud")),
            format!("safe = {} ({})", self.safe, source("safe")),
            format!("autoclean = {} ({})", self.autoclean, source("autoclean")),
        ]
    }
}

/// Reads the settings from `config` when given, otherwise from the nearest migratr.jsonc in the
/// working directory or one of its parents. With no file, every key holds its default.
pub fn load(config: Option<&Path>) -> Result<Settings, CliError> {
    let file = match config {
        Some(path) => Some(path.to_path_buf()),
        None => std::env::current_dir().ok().and_then(|cwd| {
            cwd.ancestors()
                .map(|dir| dir.join(FILE_NAME))
                .find(|path| path.is_file())
        }),
    };
    let Some(path) = file else {
        return Ok(Settings::defaults(None));
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| CliError::Usage(format!("cannot read {}: {e}", path.display())))?;
    parse(&text, path)
}

fn parse(text: &str, path: PathBuf) -> Result<Settings, CliError> {
    let shown = path.display().to_string();
    let value: Value = serde_json::from_str(&strip_comments(text))
        .map_err(|e| CliError::Usage(format!("{shown}: {e}")))?;
    let Value::Object(keys) = value else {
        return Err(CliError::Usage(format!("{shown}: expected an object")));
    };
    let mut settings = Settings::defaults(Some(path));
    for (key, value) in keys {
        let wrong = || CliError::Usage(format!("{shown}: {key} has the wrong type"));
        match key.as_str() {
            "smuggl" => settings.smuggl = Some(value.as_str().ok_or_else(wrong)?.to_string()),
            "loud" => settings.loud = value.as_bool().ok_or_else(wrong)?,
            "safe" => settings.safe = value.as_bool().ok_or_else(wrong)?,
            "autoclean" => settings.autoclean = value.as_bool().ok_or_else(wrong)?,
            _ => {
                return Err(CliError::Usage(format!(
                    "{shown}: unknown key \"{key}\"; {FILE_NAME} holds smuggl, loud, safe and autoclean"
                )));
            }
        }
        settings.set.push(key);
    }
    Ok(settings)
}

/// `text` with its `//` and `/* */` comments removed. Comment markers inside strings are kept.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => while chars.next_if(|&n| n != '\n').is_some() {},
            ('/', Some('*')) => {
                chars.next();
                let mut last = '\0';
                for n in chars.by_ref() {
                    if last == '*' && n == '/' {
                        break;
                    }
                    last = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// An executor that forwards to `inner` and, when loud, prints to stderr each migration as it
/// starts and each statement it runs. `upcoming` names the migrations in the order the run
/// applies them, one atomic run each.
pub struct Loud<E> {
    pub inner: E,
    pub loud: bool,
    pub upcoming: VecDeque<String>,
}

impl<E: Executor> Executor for Loud<E> {
    type Error = E::Error;

    fn read_schema(&mut self) -> Result<SchemaSnapshot, Self::Error> {
        self.inner.read_schema()
    }

    fn read_ledger(&mut self) -> Result<Vec<LedgerRow>, Self::Error> {
        self.inner.read_ledger()
    }

    fn foreign_key_violations(
        &mut self,
        tables: &[&str],
    ) -> Result<Option<Vec<(String, u64)>>, Self::Error> {
        self.inner.foreign_key_violations(tables)
    }

    fn missing_parents(&mut self, key: &ForeignKeyDef) -> Result<Option<u64>, Self::Error> {
        self.inner.missing_parents(key)
    }

    fn run_atomic(
        &mut self,
        statements: &[String],
        suspend_foreign_keys: bool,
    ) -> Result<(), AtomicError<Self::Error>> {
        if self.loud {
            if let Some(migration) = self.upcoming.pop_front() {
                eprintln!("migration {migration} starts");
            }
            for statement in statements {
                eprintln!("  {statement}");
            }
        }
        self.inner.run_atomic(statements, suspend_foreign_keys)
    }

    fn snapshot(&mut self, file_name: &str) -> Result<Option<PathBuf>, Self::Error> {
        self.inner.snapshot(file_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Settings {
        parse(text, PathBuf::from(FILE_NAME)).expect("parses")
    }

    #[test]
    fn comments_are_stripped_and_markers_in_strings_kept() {
        let s = parsed(
            "// the place\n{ \"smuggl\": \"https://db.example/x\", /* quiet */ \"loud\": false }",
        );
        assert_eq!(s.smuggl.as_deref(), Some("https://db.example/x"));
        assert!(!s.loud);
        assert!(s.safe && s.autoclean);
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let s = parsed(r#"{ "smuggl": "a\"//b" }"#);
        assert_eq!(s.smuggl.as_deref(), Some("a\"//b"));
    }

    #[test]
    fn a_key_the_file_sets_is_named_as_set_by_the_file() {
        let lines = parsed(r#"{ "safe": false }"#).describe();
        assert!(lines.contains(&"safe = false (set by the file)".to_string()));
        assert!(lines.contains(&"loud = true (set by the default)".to_string()));
    }

    #[test]
    fn an_unknown_key_is_a_usage_error_naming_it() {
        let Err(error) = parse(r#"{ "token": "x" }"#, PathBuf::from(FILE_NAME)) else {
            panic!("an unknown key is refused");
        };
        assert_eq!(error.code(), "usage");
        assert!(error.to_string().contains("\"token\""), "{error}");
    }
}
