//! Google Scholar search via the `scholarly` Python library.
//!
//! papr2 bundles a small Python project (`scholar/` in the source tree) and
//! writes it to `<data dir>/papr2/scholar/` (e.g. `~/.local/share/papr2/`).
//! `uv run` creates its virtual environment from the pinned `uv.lock` on
//! first use; nothing is installed system-wide.
//!
//! The helper speaks JSON on stdout; see `scholar/papr2_scholar.py`.

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use std::{
    io::Write as _,
    path::PathBuf,
    process::{Command, Stdio},
};

const SCRIPT: &str = include_str!("../scholar/papr2_scholar.py");
const PYPROJECT: &str = include_str!("../scholar/pyproject.toml");
const LOCKFILE: &str = include_str!("../scholar/uv.lock");

/// How many results a search returns.
pub const RESULTS: usize = 10;

/// One Google Scholar search result.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResult {
    pub title: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub year: String,
    #[serde(default)]
    pub venue: String,
    /// Google Scholar's snippet of the abstract (often truncated).
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    /// The publication's page.
    #[serde(default)]
    pub pub_url: String,
    /// A direct link to a PDF, when Google Scholar knows one.
    #[serde(default)]
    pub eprint_url: String,
    #[serde(default)]
    pub num_citations: u64,
    /// Opaque data the helper needs to fetch this result's BibTeX.
    #[serde(default)]
    pub raw: serde_json::Value,
}

/// Where papr2 keeps the Python project (outside the user's library).
fn project_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("papr2")
        .join("scholar")
}

/// Writes the bundled helper files if they're missing or out of date.
fn ensure_project() -> Result<PathBuf> {
    let dir = project_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, contents) in [
        ("papr2_scholar.py", SCRIPT),
        ("pyproject.toml", PYPROJECT),
        ("uv.lock", LOCKFILE),
    ] {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(contents) {
            std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(dir)
}

/// Finds `uv`. Apps started from a desktop launcher often don't get the
/// shell's `PATH`, so also look where uv's installer puts it.
pub fn find_uv() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "uv.exe" } else { "uv" };
    let on_path = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).map(|p| p.join(exe)).collect::<Vec<_>>())
        .unwrap_or_default();
    let home = dirs::home_dir().unwrap_or_default();
    on_path
        .into_iter()
        .chain([
            home.join(".local/bin").join(exe),
            home.join(".cargo/bin").join(exe),
        ])
        .find(|p| p.is_file())
}

/// Runs the helper and returns its JSON output.
fn run(args: &[&str], stdin: Option<&str>) -> Result<serde_json::Value> {
    let uv = find_uv().ok_or_else(|| {
        anyhow!(
            "Google Scholar search needs uv (https://docs.astral.sh/uv/). \
             Install it with: curl -LsSf https://astral.sh/uv/install.sh | sh"
        )
    })?;
    let dir = ensure_project()?;
    let mut child = Command::new(uv)
        .arg("run")
        .arg("--quiet")
        .arg("--project")
        .arg(&dir)
        .args(["python", "-W", "ignore"])
        .arg(dir.join("papr2_scholar.py"))
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting uv")?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(input.as_bytes()).context("sending data to the Scholar helper")?;
    }
    let output = child.wait_with_output().context("running the Scholar helper")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    match serde_json::from_str::<serde_json::Value>(last_line) {
        Ok(value) => {
            if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
                bail!("{}", explain(error));
            }
            Ok(value)
        }
        Err(_) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
            bail!("The Scholar helper failed: {detail}")
        }
    }
}

/// Adds advice to errors that usually mean Google Scholar is blocking us.
fn explain(error: &str) -> String {
    if error.contains("MaxTriesExceeded") || error.contains("Cannot Fetch") || error.contains("429") {
        format!(
            "Google Scholar refused the request; it limits automated searches. \
             Try again in a while. ({error})"
        )
    } else {
        error.to_string()
    }
}

pub fn search(query: &str) -> Result<Vec<SearchResult>> {
    let value = run(&["search", query, &RESULTS.to_string()], None)?;
    let results = value
        .get("results")
        .cloned()
        .ok_or_else(|| anyhow!("The Scholar helper returned no results field"))?;
    Ok(serde_json::from_value(results)?)
}

/// Google Scholar's BibTeX for a search result.
pub fn fetch_bibtex(result: &SearchResult) -> Result<String> {
    let value = run(&["bibtex"], Some(&result.raw.to_string()))?;
    value
        .get("bibtex")
        .and_then(|b| b.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("The Scholar helper returned no BibTeX"))
}
