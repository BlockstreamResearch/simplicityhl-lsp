use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use thiserror::Error;

const SIMFMT_BIN_NAME: &str = "simfmt";
const SIMPLEX_BIN_NAME: &str = "simplex";

#[derive(Debug, Error)]
pub enum FormatError {
    #[error(
        "`simfmt` was not found. Install it with `simplexup`, or set `simplicityhl.formatter.path`"
    )]
    NotFound,
    #[error("Unable to run `{}`: {source}", binary.display())]
    Spawn { binary: PathBuf, source: io::Error },
    #[error("`simfmt` failed to format the document:\n{0}")]
    Failed(String),
}

/// Format `text` as the contents of `document_path`, returning `None` when it is
/// already formatted.
///
/// # Errors
/// Returns a [`FormatError`] if `simfmt` cannot be found or run, or rejects `text`.
pub fn format(
    text: &str,
    document_path: &Path,
    configured_binary: &str,
) -> Result<Option<String>, FormatError> {
    // `simfmt` finds the nearest `simfmt.toml` from its working directory.
    let working_directory = document_path
        .ancestors()
        .skip(1)
        .find(|path| path.is_dir())
        .unwrap_or(Path::new("."));

    let output = run(text, working_directory, &candidates(configured_binary))?;
    if !output.status.success() {
        return Err(FormatError::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }

    let formatted =
        String::from_utf8(output.stdout).map_err(|error| FormatError::Failed(error.to_string()))?;
    Ok((formatted != text).then_some(formatted))
}

/// Binaries to try in order: the configured one alone, otherwise the `simfmt` that
/// `simplex fmt` runs, then the `simplexup` install directory, then `PATH`.
fn candidates(configured_binary: &str) -> Vec<PathBuf> {
    let configured_binary = configured_binary.trim();
    if !configured_binary.is_empty() {
        return vec![PathBuf::from(configured_binary)];
    }

    let simplex_dir = std::env::var_os("SIMPLEX_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".simplex")));

    std::env::var_os("PATH")
        .and_then(|path| simplex_sibling(&path))
        .into_iter()
        .chain(simplex_dir.map(|dir| dir.join("bin").join(SIMFMT_BIN_NAME)))
        .chain(std::iter::once(PathBuf::from(SIMFMT_BIN_NAME)))
        .collect()
}

/// The `simfmt` beside the first `simplex` on `path`, where `simplex fmt` looks for it.
fn simplex_sibling(path: &OsStr) -> Option<PathBuf> {
    let simplex = std::env::split_paths(path)
        .map(|dir| dir.join(SIMPLEX_BIN_NAME))
        .find(|simplex| simplex.is_file())?;
    // Like `simplex` itself, resolve symlinks to reach its install directory.
    let simplex = simplex.canonicalize().unwrap_or(simplex);
    Some(simplex.with_file_name(SIMFMT_BIN_NAME))
}

fn run(
    text: &str,
    working_directory: &Path,
    candidates: &[PathBuf],
) -> Result<Output, FormatError> {
    for binary in candidates {
        // `--quiet` overrides a `verbose` setting in `simfmt.toml`, which would otherwise
        // prefix the output with a `<stdin>:` header that ends up in the document.
        let child = Command::new(binary)
            .args(["--emit", "stdout", "--color", "never", "--quiet"])
            .current_dir(working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(FormatError::Spawn {
                    binary: binary.clone(),
                    source,
                })
            }
        };

        let mut stdin = child.stdin.take().expect("stdin is piped");
        let output = std::thread::scope(|scope| {
            scope.spawn(move || stdin.write_all(text.as_bytes()));
            child.wait_with_output()
        });

        return output.map_err(|source| FormatError::Spawn {
            binary: binary.clone(),
            source,
        });
    }

    Err(FormatError::NotFound)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn configured_binary_is_used_alone() {
        let temp = tempfile::TempDir::new().unwrap();
        let missing = temp.path().join("simfmt");

        assert_eq!(candidates(" /opt/simfmt "), [PathBuf::from("/opt/simfmt")]);
        assert!(matches!(
            format(
                "",
                &temp.path().join("main.simf"),
                missing.to_str().unwrap()
            ),
            Err(FormatError::NotFound)
        ));
    }

    #[test]
    fn prefers_the_simfmt_beside_simplex_on_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let (empty, bin) = (temp.path().join("empty"), temp.path().join("bin"));

        fs::create_dir(&empty).unwrap();
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join(SIMPLEX_BIN_NAME), "").unwrap();
        let path = std::env::join_paths([&empty, &bin]).unwrap();

        assert_eq!(
            simplex_sibling(&path),
            Some(bin.canonicalize().unwrap().join(SIMFMT_BIN_NAME))
        );
        assert_eq!(simplex_sibling(empty.as_os_str()), None);
    }

    /// Formats `contracts/main.simf` with a shell script standing in for `simfmt`.
    #[cfg(unix)]
    fn format_with(script: &str, text: &str) -> Result<Option<String>, FormatError> {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir(temp.path().join("contracts")).unwrap();
        let binary = temp.path().join("simfmt");
        // A child shell writes the script: a write handle held by this process could leak
        // into a concurrent test's child and make running it fail with "Text file busy".
        let status = Command::new("sh")
            .args([
                "-c",
                r#"printf '#!/bin/sh\n%s\n' "$1" > "$0" && chmod 755 "$0""#,
            ])
            .arg(&binary)
            .arg(script)
            .status()
            .unwrap();
        assert!(status.success());

        format(
            text,
            &temp.path().join("contracts/main.simf"),
            binary.to_str().unwrap(),
        )
    }

    #[cfg(unix)]
    #[test]
    fn formats_from_the_document_directory() {
        let formatted = format_with("cat >/dev/null; pwd", "fn main() {}\n").unwrap();

        assert!(formatted.unwrap().trim_end().ends_with("/contracts"));
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_text_needs_no_edit() {
        // Like `simfmt` with `verbose` in `simfmt.toml`: a `<stdin>:` header unless `--quiet`.
        let simfmt = r#"case " $* " in *" --quiet "*) ;; *) printf '<stdin>:\n\n' ;; esac; cat"#;

        assert_eq!(format_with(simfmt, "fn main() {}\n").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn rejected_input_reports_stderr() {
        let error = format_with(
            "cat >/dev/null; echo 'Unclosed (' >&2; exit 1",
            "fn main( {}\n",
        )
        .unwrap_err();

        assert!(matches!(&error, FormatError::Failed(stderr) if stderr == "Unclosed ("));
    }
}
