//! The `embed!` macro behind `migratr::embed!`.

use std::fs;
use std::path::{Path, PathBuf};

use proc_macro::{Delimiter, TokenStream, TokenTree};

// The macro validates a directory with the same parser the library runs. The library depends
// on this crate, so the parser's source file is shared rather than depended on.
#[allow(dead_code)]
#[path = "../../migratr/src/format.rs"]
mod format;

/// Embeds the migrations directory named by the string literal, relative to the calling
/// crate's manifest directory, and expands to a `migratr::Migrator`. A file that does not
/// parse is a compile error naming it. Adding a file to the directory does not by itself
/// trigger a rebuild; editing an existing file does.
#[proc_macro]
pub fn embed(input: TokenStream) -> TokenStream {
    match expand(input) {
        Ok(tokens) => tokens,
        Err(message) => format!("::core::compile_error!({message:?})")
            .parse()
            .unwrap_or_default(),
    }
}

fn expand(input: TokenStream) -> Result<TokenStream, String> {
    let relative = directory_literal(input)?;
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .ok_or_else(|| "embed!: CARGO_MANIFEST_DIR is not set".to_string())?;
    let dir = PathBuf::from(manifest).join(&relative);

    // The migration files: every `.json` file but `schema.json`, as the library reads them.
    let io = |path: &Path, e: std::io::Error| format!("embed!: {}: {e}", path.display());
    let mut files: Vec<(String, String)> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| io(&dir, e))? {
        let path = entry.map_err(|e| io(&dir, e))?.path();
        let file = path
            .file_name()
            .and_then(|f| f.to_str())
            .ok_or_else(|| format!("embed!: {} is not valid UTF-8", path.display()))?
            .to_string();
        if path.extension().and_then(|e| e.to_str()) != Some("json") || file == "schema.json" {
            continue;
        }
        let contents = fs::read_to_string(&path).map_err(|e| io(&path, e))?;
        files.push((file, contents));
    }
    files.sort();

    let sources: Vec<(&str, &str)> = files
        .iter()
        .map(|(file, contents)| (file.as_str(), contents.as_str()))
        .collect();
    format::parse_all(&sources).map_err(|e| format!("embed!: {e}"))?;

    let mut entries = String::new();
    for (file, _) in &files {
        let path = dir.join(file);
        let path = path
            .to_str()
            .ok_or_else(|| format!("embed!: {} is not valid UTF-8", path.display()))?;
        entries.push_str(&format!("({file:?}, include_str!({path:?})),"));
    }

    // The files parsed above, and the library parses them with the same code.
    format!("::migratr::Migrator::from_validated(&[{entries}])")
        .parse()
        .map_err(|e| format!("embed!: {e}"))
}

/// The single plain string literal the macro was given.
fn directory_literal(input: TokenStream) -> Result<String, String> {
    let usage = "embed! takes one string literal, the migrations directory";
    let mut tokens = input.into_iter().filter(|t| {
        !matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::None && g.stream().is_empty())
    });
    let (Some(TokenTree::Literal(literal)), None) = (tokens.next(), tokens.next()) else {
        return Err(usage.to_string());
    };
    let text = literal.to_string();
    let inner = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .ok_or_else(|| usage.to_string())?;
    if inner.contains('\\') {
        return Err("embed!: the directory literal must not contain escapes".to_string());
    }
    Ok(inner.to_string())
}
