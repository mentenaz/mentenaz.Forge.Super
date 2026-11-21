//! Python runtime backend — environment detection, project scanning, dependency
//! checking, framework detection, and package management.
//!
//! No GPUI, no app-state dependency: every function here takes plain
//! strings/paths and returns a plain value or `Result` — a host panel wires
//! this into its own UI/state, this crate just does the work.

use std::path::Path;

use serde::{Deserialize, Serialize};

mod path_env;

// ── Types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PythonPackage {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PythonFramework {
    pub name: String,
    pub icon: String,
    pub dev_command: String,
    pub port: Option<u16>,
}

/// Package metadata fetched from the PyPI JSON API
/// (`https://pypi.org/pypi/<name>/json` or `/<name>/<version>/json`).
#[derive(Clone, Debug, Default)]
pub struct PyPiPackageInfo {
    pub name: String,
    pub version: String,
    pub summary: Option<String>,
    /// `info.description` — usually the full README content.
    pub readme: Option<String>,
    /// `info.description_content_type` — e.g. `text/markdown`, `text/x-rst`.
    pub readme_content_type: Option<String>,
    pub author: Option<String>,
    pub license: Option<String>,
    pub home_page: Option<String>,
    pub project_urls: Vec<(String, String)>,
    pub requires_python: Option<String>,
    pub requires_dist: Vec<String>,
    /// Python versions this release supports, parsed out of `classifiers`
    /// (e.g. "Programming Language :: Python :: 3.11").
    pub python_versions: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PythonProjectScan {
    pub venvs: Vec<String>,
    pub requirements: Vec<String>,
    pub entry_points: Vec<(String, String)>, // (file, dir)
}

pub const FRAMEWORK_MARKERS: &[(&str, &str, &str, Option<u16>)] = &[
    ("flask", "Flask", "flask run --debug", Some(5000)),
    ("fastapi", "FastAPI", "uvicorn main:app --reload", Some(8000)),
    ("django", "Django", "python manage.py runserver 0.0.0.0:8000", Some(8000)),
    ("streamlit", "Streamlit", "streamlit run app.py", Some(8501)),
    ("gradio", "Gradio", "python app.py", Some(7860)),
    ("pytest", "pytest", "python -m pytest -v", None),
];

pub const PYTHON_ENTRY_POINTS: &[&str] = &[
    "app.py",
    "main.py",
    "run.py",
    "server.py",
    "manage.py",
    "wsgi.py",
    "asgi.py",
];

const SCAN_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".forge",
    "dist",
    "build",
    "target",
    "__pycache__",
    ".next",
    "coverage",
    ".cache",
    "out",
    "venv",
    "env",
    ".venv",
];

const MAX_SCAN_DEPTH: usize = 5;

// ── Runtime queries ───────────────────────────────────────────────────

/// Run a command and return stdout (or stderr if stdout is empty).
fn run_captured(exe: &str, args: &[&str]) -> Result<String, String> {
    // `exe` is often the project's own virtual environment. It is resolved first
    // (a bare name through `PATH`) and the file it names is judged, before
    // anything is spawned, so a program inside a project that is not trusted is
    // not started.
    forge_trust_gate::check_run("python_backend::run_captured", exe, args, None)?;
    let mut c = gpui_util::new_std_command(exe);
    c.args(args);
    #[cfg(target_os = "windows")]
    path_env::enrich_path(&mut c);
    let out = match c.output() {
        Ok(out) => out,
        Err(e) => {
            log::debug!("run_captured: direct '{exe}' failed: {e}");
            #[cfg(target_os = "windows")]
            {
                let mut sh = gpui_util::new_std_command("cmd");
                sh.args(["/C", exe]);
                sh.args(args);
                path_env::enrich_path(&mut sh);
                match sh.output() {
                    Ok(fb) => fb,
                    Err(e2) => {
                        log::debug!("run_captured: cmd /C '{exe}' also failed: {e2}");
                        return Err(format!("{exe} not found"));
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            return Err(format!("{exe} not found"));
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok(if stdout.is_empty() { stderr } else { stdout })
}

/// Query Python version with `python --version`.
pub fn query_python(exe: &str) -> Result<String, String> {
    run_captured(exe, &["--version"])
}

/// Query pip version with `python -m pip --version`.
pub fn query_pip(exe: &str) -> Result<String, String> {
    run_captured(exe, &["-m", "pip", "--version"])
}

/// List installed packages as JSON: `python -m pip list --format json`.
pub fn list_packages(exe: &str) -> Result<Vec<PythonPackage>, String> {
    let output = run_captured(exe, &["-m", "pip", "list", "--format", "json"])?;
    parse_pip_list(&output)
}

fn parse_pip_list(output: &str) -> Result<Vec<PythonPackage>, String> {
    let parsed: Vec<serde_json::Value> = serde_json::from_str(output)
        .map_err(|e| format!("Failed to parse pip output: {e}"))?;

    let packages = parsed
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            let version = p.get("version")?.as_str()?;
            Some(PythonPackage {
                name: name.to_lowercase(),
                version: version.to_string(),
            })
        })
        .collect();

    Ok(packages)
}

/// Which part of a package's version moved between the installed and latest
/// releases — drives the severity badge on Outdated/Updates sections.
/// Mirrors `dotnet_backend::UpdateKind` (same three-tier logic, same
/// "unparseable or equal defaults to `Patch`" fallback) so both ecosystems
/// classify risk identically.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum UpdateKind {
    Major,
    Minor,
    Patch,
}

impl UpdateKind {
    pub fn label(self) -> &'static str {
        match self {
            UpdateKind::Major => "major",
            UpdateKind::Minor => "minor",
            UpdateKind::Patch => "patch",
        }
    }
}

/// Parses a version string's leading `major.minor.patch` numeric run,
/// ignoring anything after the first non-numeric/non-dot segment (pre-release
/// suffixes like `2.0.0rc1`, PEP 440 local versions like `1.0+local`, etc.).
fn version_parts(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v
        .split(|c: char| c == '.' || c == '-' || c == '+')
        .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<u64>().ok());
    let major = parts.next()?;
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    Some((major, minor, patch))
}

/// Classifies an installed→latest version jump. Matches
/// `dotnet_backend::classify_update`'s exact fallback: unparseable or equal
/// versions default to `Patch` (the least alarming classification), not
/// `Major` — since a version pip already reports as "outdated" but this
/// parser can't compare shouldn't read as a major-risk change by default.
pub fn classify_update(installed: &str, latest: &str) -> UpdateKind {
    match (version_parts(installed), version_parts(latest)) {
        (Some(inst), Some(lat)) if inst != lat => {
            if inst.0 != lat.0 {
                UpdateKind::Major
            } else if inst.1 != lat.1 {
                UpdateKind::Minor
            } else {
                UpdateKind::Patch
            }
        }
        _ => UpdateKind::Patch,
    }
}

/// One row from `pip list --outdated --format json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PythonOutdatedPkg {
    pub name: String,
    pub version: String,
    pub latest_version: String,
    pub kind: UpdateKind,
}

/// List outdated installed packages: `python -m pip list --outdated --format json`.
pub fn list_outdated(exe: &str) -> Result<Vec<PythonOutdatedPkg>, String> {
    let output = run_captured(exe, &["-m", "pip", "list", "--outdated", "--format", "json"])?;
    parse_pip_outdated(&output)
}

fn parse_pip_outdated(output: &str) -> Result<Vec<PythonOutdatedPkg>, String> {
    let parsed: Vec<serde_json::Value> = serde_json::from_str(output)
        .map_err(|e| format!("Failed to parse pip output: {e}"))?;

    let packages = parsed
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?;
            let version = p.get("version")?.as_str()?;
            let latest_version = p.get("latest_version")?.as_str()?;
            Some(PythonOutdatedPkg {
                name: name.to_lowercase(),
                version: version.to_string(),
                latest_version: latest_version.to_string(),
                kind: classify_update(version, latest_version),
            })
        })
        .collect();

    Ok(packages)
}

// ── PyPI metadata ──────────────────────────────────────────────────────

/// Parses a `https://pypi.org/pypi/<name>/json` response body into
/// [`PyPiPackageInfo`]. Pure parsing — the HTTP fetch itself is the caller's
/// job (it needs an async HTTP client, which is a UI/app-layer concern this
/// crate deliberately has no opinion on).
pub fn parse_pypi_json(body: &serde_json::Value) -> Result<PyPiPackageInfo, String> {
    let info = body
        .get("info")
        .ok_or_else(|| "missing 'info' in PyPI response".to_string())?;

    let name = info
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'info.name'".to_string())?
        .to_string();
    let version = info
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let non_empty = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(String::from);

    let summary = non_empty(info.get("summary").and_then(|v| v.as_str()));
    let readme = non_empty(info.get("description").and_then(|v| v.as_str()));
    let readme_content_type = non_empty(info.get("description_content_type").and_then(|v| v.as_str()));
    let author = non_empty(info.get("author").and_then(|v| v.as_str()));
    let license = non_empty(info.get("license").and_then(|v| v.as_str()));
    let home_page = non_empty(info.get("home_page").and_then(|v| v.as_str()))
        .or_else(|| non_empty(info.get("project_url").and_then(|v| v.as_str())));
    let requires_python = non_empty(info.get("requires_python").and_then(|v| v.as_str()));

    let project_urls = info
        .get("project_urls")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let requires_dist = info
        .get("requires_dist")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let python_versions = info
        .get("classifiers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| s.strip_prefix("Programming Language :: Python :: "))
                .filter(|s| *s != "3" && *s != "2" && !s.starts_with("Implementation"))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    Ok(PyPiPackageInfo {
        name,
        version,
        summary,
        readme,
        readme_content_type,
        author,
        license,
        home_page,
        project_urls,
        requires_python,
        requires_dist,
        python_versions,
    })
}

/// One entry from a `https://pypi.org/pypi/<name>/<version>/json` response's
/// top-level `vulnerabilities` array — PyPI surfaces these straight from
/// OSV.dev (https://osv.dev) for the *exact* release requested, so unlike
/// `PyPiPackageInfo` (which comes off the version-less `/pypi/<name>/json`
/// endpoint and only ever describes the latest release) this has to be
/// fetched per installed package at its installed version.
#[derive(Clone, Debug, Default)]
pub struct PyPiVulnerability {
    /// OSV/PyPI advisory id, e.g. "PYSEC-2023-74" or "GHSA-...".
    pub id: String,
    pub summary: Option<String>,
    pub details: Option<String>,
    /// Other identifiers for the same advisory, e.g. "CVE-2023-32681".
    pub aliases: Vec<String>,
    /// Versions this vulnerability is fixed in, if known.
    pub fixed_in: Vec<String>,
    pub link: Option<String>,
    pub source: Option<String>,
}

/// Parses the `vulnerabilities` array out of a per-version PyPI JSON
/// response. Withdrawn advisories (later retracted by OSV) are dropped.
pub fn parse_pypi_vulnerabilities(body: &serde_json::Value) -> Vec<PyPiVulnerability> {
    let non_empty = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let str_list = |v: &serde_json::Value, key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };

    body.get("vulnerabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|v| v.get("withdrawn").map(|w| w.is_null()).unwrap_or(true))
                .filter_map(|v| {
                    let id = v.get("id")?.as_str()?.to_string();
                    Some(PyPiVulnerability {
                        id,
                        summary: non_empty(v.get("summary").and_then(|s| s.as_str())),
                        details: non_empty(v.get("details").and_then(|s| s.as_str())),
                        aliases: str_list(v, "aliases"),
                        fixed_in: str_list(v, "fixed_in"),
                        link: non_empty(v.get("link").and_then(|s| s.as_str())),
                        source: non_empty(v.get("source").and_then(|s| s.as_str())),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── Multi-project detection ─────────────────────────────────────────────

/// Which dependency-declaration file a detected project uses to define its
/// environment. Checked in this priority order when a project has more than
/// one — `requirements.txt` wins because it's the simplest, most explicit
/// case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvMarker {
    Requirements,
    Pyproject,
    Pipfile,
}

impl EnvMarker {
    pub fn file_name(self) -> &'static str {
        match self {
            EnvMarker::Requirements => "requirements.txt",
            EnvMarker::Pyproject => "pyproject.toml",
            EnvMarker::Pipfile => "Pipfile",
        }
    }

    /// The command that bootstraps a venv (or, for Pipfile, a pipenv-managed
    /// environment) from this marker file, run from the project directory
    /// with `exe` as the interpreter used to create the venv.
    pub fn create_command(self, exe: &str) -> String {
        // Where a freshly created venv puts its interpreter differs by OS.
        #[cfg(target_os = "windows")]
        const VENV_PYTHON: &str = "venv\\Scripts\\python.exe";
        #[cfg(not(target_os = "windows"))]
        const VENV_PYTHON: &str = "venv/bin/python";

        match self {
            EnvMarker::Requirements => format!(
                "{exe} -m venv venv && {VENV_PYTHON} -m pip install -r requirements.txt"
            ),
            // `pip install -e .` reads build-system deps straight out of
            // pyproject.toml without requiring Poetry/PDM/etc. to be
            // installed — works for any PEP 517 backend.
            EnvMarker::Pyproject => {
                format!("{exe} -m venv venv && {VENV_PYTHON} -m pip install -e .")
            }
            // Pipenv manages its own venv (not this project's `venv\`
            // folder) — there's nothing else to scope pip/venv lookups to
            // afterwards, but it's still the correct one-command bootstrap
            // for a Pipfile.
            EnvMarker::Pipfile => format!("{exe} -m pip install pipenv && {exe} -m pipenv install"),
        }
    }
}

/// A Python sub-project detected inside the workspace: any directory that
/// declares its dependencies via `requirements.txt`, `pyproject.toml`, or
/// `Pipfile`. Mirrors `NpmProject`/`node_backend::DetectedNodeProject` — lets
/// a monorepo-style workspace pick which project's venv/pip commands a panel
/// operates on, instead of guessing from a single workspace-wide scan.
/// Shared by `python_manager_panel` (its project switcher) and `python_panel`
/// (its Projects section) — moved here from `python_manager_panel` so both
/// can use one scanner instead of two.
#[derive(Clone, Debug)]
pub struct DetectedPythonProject {
    pub path: String,
    pub name: String,
    pub marker: EnvMarker,
}

const PROJECT_SCAN_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    "__pycache__",
    ".next",
    "coverage",
    ".cache",
    "out",
    "venv",
    "env",
    ".venv",
];
const PROJECT_SCAN_MAX_DEPTH: usize = 5;

pub fn scan_python_projects(root: &str, depth: usize) -> Vec<DetectedPythonProject> {
    if depth > PROJECT_SCAN_MAX_DEPTH {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let entries: Vec<_> = entries.flatten().collect();

    let mut projects = Vec::new();

    let has = |file: &str| {
        entries
            .iter()
            .any(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(file))
    };
    let marker = if has("requirements.txt") {
        Some(EnvMarker::Requirements)
    } else if has("pyproject.toml") {
        Some(EnvMarker::Pyproject)
    } else if has("Pipfile") {
        Some(EnvMarker::Pipfile)
    } else {
        None
    };
    if let Some(marker) = marker {
        let name = Path::new(root)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string());
        projects.push(DetectedPythonProject {
            path: root.to_string(),
            name,
            marker,
        });
    }

    for entry in &entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if PROJECT_SCAN_SKIP_DIRS
            .iter()
            .any(|skip| *skip == dir_name.to_lowercase())
        {
            continue;
        }
        projects.extend(scan_python_projects(&path.to_string_lossy(), depth + 1));
    }

    projects
}

// ── Project scanning ───────────────────────────────────────────────────

pub fn scan_python_project(root: &str) -> Result<PythonProjectScan, String> {
    let mut result = PythonProjectScan {
        venvs: Vec::new(),
        requirements: Vec::new(),
        entry_points: Vec::new(),
    };
    walk(Path::new(root), 0, &mut result)?;
    Ok(result)
}

fn walk(dir: &Path, depth: usize, out: &mut PythonProjectScan) -> Result<(), String> {
    if depth > MAX_SCAN_DEPTH {
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();

            // Check for venv/env directories
            if name == "venv" || name == "env" || name == ".venv" {
                for exe in &[
                    format!("{}\\Scripts\\python.exe", path.display()),
                    format!("{}/bin/python", path.display()),
                ] {
                    if Path::new(exe).exists() {
                        out.venvs.push(exe.clone());
                    }
                }
                // Don't recurse into venv directories
                continue;
            }

            if !SCAN_SKIP_DIRS.iter().any(|skip| skip == &name.as_str()) {
                walk(&path, depth + 1, out)?;
            }
        } else if path.is_file() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();

            if name == "requirements.txt" {
                out.requirements.push(path.to_string_lossy().into_owned());
            } else if PYTHON_ENTRY_POINTS.iter().any(|ep| ep == &name) {
                let dir = path
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.entry_points.push((path.to_string_lossy().into_owned(), dir));
            }
        }
    }

    Ok(())
}

// ── Framework detection ────────────────────────────────────────────────

pub fn detect_framework(
    requirements: &[String],
    entry_points: &[(String, String)],
) -> Option<PythonFramework> {
    // Read requirements.txt files
    let mut req_content = String::new();
    for req_path in requirements {
        if let Ok(content) = std::fs::read_to_string(req_path) {
            req_content.push(' ');
            req_content.push_str(&content.to_lowercase());
        }
    }

    // Get entry point names
    let entry_names: Vec<String> = entry_points
        .iter()
        .map(|(file, _)| {
            Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase()
        })
        .collect();

    let search_str = format!("{} {}", req_content, entry_names.join(" "));

    // Check for framework markers
    for (marker, name, dev_cmd, port) in FRAMEWORK_MARKERS {
        if search_str.contains(marker) {
            return Some(PythonFramework {
                name: name.to_string(),
                icon: match *name {
                    "Flask" => "🌶",
                    "FastAPI" => "⚡",
                    "Django" => "🎸",
                    "Streamlit" => "🎈",
                    "Gradio" => "🤗",
                    "pytest" => "🧪",
                    _ => "🐍",
                }
                .to_string(),
                dev_command: dev_cmd.to_string(),
                port: *port,
            });
        }
    }

    None
}

/// Find missing dependencies by comparing requirements.txt with installed packages.
pub fn find_missing_deps(
    requirements_path: Option<&str>,
    installed: &[PythonPackage],
) -> Result<Vec<String>, String> {
    if let Some(path) = requirements_path {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read requirements.txt: {e}"))?;

        let installed_names: std::collections::HashSet<_> =
            installed.iter().map(|p| p.name.as_str()).collect();

        let missing: Vec<String> = content
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                // Parse package name (before any operators like ==, >=, <, etc.)
                line.split(|c: char| c == '<' || c == '>' || c == '=' || c == '~' || c == '!')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_lowercase()
            })
            .filter(|pkg| !pkg.is_empty() && !installed_names.contains(pkg.as_str()))
            .collect();

        Ok(missing)
    } else {
        Ok(Vec::new())
    }
}

/// Count dependencies from requirements.txt.
pub fn count_requirements(path: Option<&str>) -> Result<usize, String> {
    if let Some(p) = path {
        let content = std::fs::read_to_string(p)
            .map_err(|e| format!("Failed to read requirements.txt: {e}"))?;
        let count = content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .count();
        Ok(count)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn package(name: &str) -> PythonPackage {
        PythonPackage {
            name: name.to_string(),
            version: "1.0.0".to_string(),
        }
    }

    #[test]
    fn pip_list_lowercases_names_and_skips_malformed_rows() {
        let output = r#"[
            {"name": "Flask", "version": "3.0.2"},
            {"name": "requests", "version": "2.31.0"},
            {"name": "no-version"},
            {"version": "1.0.0"}
        ]"#;
        let packages = parse_pip_list(output).unwrap();
        let summary: Vec<_> = packages
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(summary, vec![("flask", "3.0.2"), ("requests", "2.31.0")]);

        assert!(parse_pip_list("[]").unwrap().is_empty());
        assert!(parse_pip_list("WARNING: pip is out of date").is_err());
    }

    #[test]
    fn pip_outdated_classifies_each_row() {
        let output = r#"[
            {"name": "Django", "version": "4.2.0", "latest_version": "5.0.1", "latest_filetype": "wheel"},
            {"name": "urllib3", "version": "2.1.0", "latest_version": "2.2.0"},
            {"name": "idna", "version": "3.6", "latest_version": "3.6.1"},
            {"name": "incomplete", "version": "1.0.0"}
        ]"#;
        let outdated = parse_pip_outdated(output).unwrap();
        let summary: Vec<_> = outdated
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str(), p.latest_version.as_str(), p.kind))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("django", "4.2.0", "5.0.1", UpdateKind::Major),
                ("urllib3", "2.1.0", "2.2.0", UpdateKind::Minor),
                ("idna", "3.6", "3.6.1", UpdateKind::Patch),
            ]
        );
        assert!(parse_pip_outdated("{").is_err());
    }

    #[test]
    fn classify_update_by_changed_component() {
        assert_eq!(classify_update("1.2.3", "2.0.0"), UpdateKind::Major);
        assert_eq!(classify_update("1.2.3", "1.3.0"), UpdateKind::Minor);
        assert_eq!(classify_update("1.2.3", "1.2.4"), UpdateKind::Patch);
        assert_eq!(classify_update("1.2", "1.3"), UpdateKind::Minor);
    }

    #[test]
    fn classify_update_ignores_pep440_suffixes() {
        // `2.0.0rc1` and `1.0+local` compare as their numeric release part.
        assert_eq!(classify_update("1.9.0", "2.0.0rc1"), UpdateKind::Major);
        assert_eq!(classify_update("1.0+local", "1.1"), UpdateKind::Minor);
        assert_eq!(classify_update("2.0.0rc1", "2.0.0"), UpdateKind::Patch);
    }

    #[test]
    fn classify_update_falls_back_to_patch() {
        assert_eq!(classify_update("1.2.3", "1.2.3"), UpdateKind::Patch);
        assert_eq!(classify_update("unknown", "2.0.0"), UpdateKind::Patch);
        assert_eq!(classify_update("1.0.0", ""), UpdateKind::Patch);
    }

    #[test]
    fn update_kind_labels() {
        assert_eq!(UpdateKind::Major.label(), "major");
        assert_eq!(UpdateKind::Minor.label(), "minor");
        assert_eq!(UpdateKind::Patch.label(), "patch");
    }

    #[test]
    fn pypi_json_full_record() {
        let body = json!({
            "info": {
                "name": "requests",
                "version": "2.31.0",
                "summary": "  Python HTTP for Humans.  ",
                "description": "# Requests",
                "description_content_type": "text/markdown",
                "author": "Kenneth Reitz",
                "license": "Apache 2.0",
                "home_page": "https://requests.readthedocs.io",
                "project_url": "https://pypi.org/project/requests/",
                "requires_python": ">=3.7",
                "project_urls": {
                    "Source": "https://github.com/psf/requests",
                    "Broken": null
                },
                "requires_dist": ["idna<4,>=2.5", "urllib3<3,>=1.21.1", 7],
                "classifiers": [
                    "License :: OSI Approved :: Apache Software License",
                    "Programming Language :: Python :: 3",
                    "Programming Language :: Python :: 3.11",
                    "Programming Language :: Python :: 3.12",
                    "Programming Language :: Python :: Implementation :: CPython"
                ]
            }
        });

        let info = parse_pypi_json(&body).unwrap();
        assert_eq!(info.name, "requests");
        assert_eq!(info.version, "2.31.0");
        assert_eq!(info.summary.as_deref(), Some("Python HTTP for Humans."));
        assert_eq!(info.readme.as_deref(), Some("# Requests"));
        assert_eq!(info.readme_content_type.as_deref(), Some("text/markdown"));
        assert_eq!(info.author.as_deref(), Some("Kenneth Reitz"));
        assert_eq!(info.license.as_deref(), Some("Apache 2.0"));
        assert_eq!(info.home_page.as_deref(), Some("https://requests.readthedocs.io"));
        assert_eq!(info.requires_python.as_deref(), Some(">=3.7"));
        assert_eq!(info.requires_dist, vec!["idna<4,>=2.5", "urllib3<3,>=1.21.1"]);
        assert_eq!(info.python_versions, vec!["3.11", "3.12"]);
        assert_eq!(info.project_urls.len(), 1);
        assert!(
            info.project_urls
                .iter()
                .any(|(label, url)| label == "Source" && url == "https://github.com/psf/requests")
        );
    }

    #[test]
    fn pypi_json_blank_fields_become_none() {
        let body = json!({
            "info": {
                "name": "sparse",
                "summary": "   ",
                "description": "",
                "author": null,
                "home_page": "",
                "project_url": "https://pypi.org/project/sparse/"
            }
        });

        let info = parse_pypi_json(&body).unwrap();
        assert_eq!(info.version, "");
        assert_eq!(info.summary, None);
        assert_eq!(info.readme, None);
        assert_eq!(info.author, None);
        assert_eq!(info.license, None);
        // An empty `home_page` falls back to the PyPI project page.
        assert_eq!(info.home_page.as_deref(), Some("https://pypi.org/project/sparse/"));
        assert!(info.requires_dist.is_empty());
        assert!(info.python_versions.is_empty());
        assert!(info.project_urls.is_empty());
    }

    #[test]
    fn pypi_json_requires_info_and_name() {
        assert!(parse_pypi_json(&json!({ "message": "Not Found" })).is_err());
        assert!(parse_pypi_json(&json!({ "info": { "version": "1.0.0" } })).is_err());
    }

    #[test]
    fn pypi_vulnerabilities_drop_withdrawn_and_idless_entries() {
        let body = json!({
            "vulnerabilities": [
                {
                    "id": "PYSEC-2023-74",
                    "summary": "Leaks Proxy-Authorization",
                    "details": "  ",
                    "aliases": ["CVE-2023-32681", "GHSA-j8r2-6x86-q33q"],
                    "fixed_in": ["2.31.0"],
                    "link": "https://osv.dev/vulnerability/PYSEC-2023-74",
                    "source": "osv",
                    "withdrawn": null
                },
                { "id": "PYSEC-RETRACTED", "withdrawn": "2024-01-01T00:00:00Z" },
                { "summary": "no id" },
                { "id": "GHSA-minimal" }
            ]
        });

        let vulnerabilities = parse_pypi_vulnerabilities(&body);
        let ids: Vec<_> = vulnerabilities.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, vec!["PYSEC-2023-74", "GHSA-minimal"]);

        let first = &vulnerabilities[0];
        assert_eq!(first.summary.as_deref(), Some("Leaks Proxy-Authorization"));
        assert_eq!(first.details, None);
        assert_eq!(first.aliases, vec!["CVE-2023-32681", "GHSA-j8r2-6x86-q33q"]);
        assert_eq!(first.fixed_in, vec!["2.31.0"]);
        assert_eq!(first.link.as_deref(), Some("https://osv.dev/vulnerability/PYSEC-2023-74"));
        assert_eq!(first.source.as_deref(), Some("osv"));

        let minimal = &vulnerabilities[1];
        assert_eq!(minimal.summary, None);
        assert!(minimal.aliases.is_empty());
        assert!(minimal.fixed_in.is_empty());

        assert!(parse_pypi_vulnerabilities(&json!({ "info": {} })).is_empty());
        assert!(parse_pypi_vulnerabilities(&json!({ "vulnerabilities": [] })).is_empty());
    }

    #[test]
    fn env_marker_files_and_commands() {
        assert_eq!(EnvMarker::Requirements.file_name(), "requirements.txt");
        assert_eq!(EnvMarker::Pyproject.file_name(), "pyproject.toml");
        assert_eq!(EnvMarker::Pipfile.file_name(), "Pipfile");

        let venv_python = if cfg!(target_os = "windows") {
            "venv\\Scripts\\python.exe"
        } else {
            "venv/bin/python"
        };
        assert_eq!(
            EnvMarker::Requirements.create_command("py"),
            format!("py -m venv venv && {venv_python} -m pip install -r requirements.txt")
        );
        assert_eq!(
            EnvMarker::Pyproject.create_command("py"),
            format!("py -m venv venv && {venv_python} -m pip install -e .")
        );
        assert_eq!(
            EnvMarker::Pipfile.create_command("py"),
            "py -m pip install pipenv && py -m pipenv install"
        );
    }

    #[test]
    fn project_scan_picks_one_marker_per_directory_by_priority() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("requirements.txt"), "");
        write(&root.join("pyproject.toml"), "");
        write(&root.join("services/api/pyproject.toml"), "");
        write(&root.join("services/api/Pipfile"), "");
        write(&root.join("services/worker/Pipfile"), "");
        write(&root.join("docs/readme.md"), "");
        // Environments and build output are never reported as projects.
        write(&root.join("venv/requirements.txt"), "");
        write(&root.join(".venv/pyproject.toml"), "");
        write(&root.join("node_modules/pkg/requirements.txt"), "");

        let mut projects = scan_python_projects(&root.to_string_lossy(), 0);
        projects.sort_by(|a, b| a.path.cmp(&b.path));
        let markers: Vec<_> = projects.iter().map(|p| p.marker).collect();
        assert_eq!(
            markers,
            vec![EnvMarker::Requirements, EnvMarker::Pyproject, EnvMarker::Pipfile]
        );

        assert_eq!(projects[0].path, root.to_string_lossy());
        assert_eq!(
            projects[0].name,
            root.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(projects[1].name, "api");
        assert_eq!(projects[2].name, "worker");
    }

    #[test]
    fn project_scan_respects_depth_and_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("requirements.txt"), "");
        let root = dir.path().to_string_lossy().into_owned();

        assert_eq!(scan_python_projects(&root, PROJECT_SCAN_MAX_DEPTH).len(), 1);
        assert!(scan_python_projects(&root, PROJECT_SCAN_MAX_DEPTH + 1).is_empty());
        assert!(scan_python_projects(&dir.path().join("missing").to_string_lossy(), 0).is_empty());
    }

    #[test]
    fn single_project_scan_collects_requirements_entry_points_and_venvs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("requirements.txt"), "flask\n");
        write(&root.join("app.py"), "");
        write(&root.join("helpers.py"), "");
        write(&root.join("api/main.py"), "");
        write(&root.join("api/requirements.txt"), "");
        write(&root.join("venv/bin/python"), "");
        // Nothing inside an environment or a skipped directory is collected.
        write(&root.join("venv/lib/app.py"), "");
        write(&root.join("__pycache__/main.py"), "");
        write(&root.join("build/requirements.txt"), "");

        let scan = scan_python_project(&root.to_string_lossy()).unwrap();

        let mut requirements = scan.requirements.clone();
        requirements.sort();
        assert_eq!(
            requirements,
            {
                let mut expected = vec![
                    root.join("requirements.txt").to_string_lossy().into_owned(),
                    root.join("api").join("requirements.txt").to_string_lossy().into_owned(),
                ];
                expected.sort();
                expected
            }
        );

        let mut entry_files: Vec<String> = scan
            .entry_points
            .iter()
            .map(|(file, _)| {
                Path::new(file)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        entry_files.sort();
        assert_eq!(entry_files, vec!["app.py", "main.py"]);
        for (file, dir) in &scan.entry_points {
            assert_eq!(Path::new(file).parent().unwrap(), Path::new(dir));
        }

        assert_eq!(scan.venvs.len(), 1);
        assert!(scan.venvs[0].ends_with("/bin/python"));
    }

    #[test]
    fn framework_detected_from_requirements() {
        let dir = tempfile::tempdir().unwrap();
        let flask = dir.path().join("flask-requirements.txt");
        let django = dir.path().join("django-requirements.txt");
        let plain = dir.path().join("plain-requirements.txt");
        write(&flask, "Flask==3.0.2\nrequests\n");
        write(&django, "Django>=5.0\n");
        write(&plain, "requests\nnumpy\n");
        let path = |p: &Path| p.to_string_lossy().into_owned();

        let framework = detect_framework(&[path(&flask)], &[]).unwrap();
        assert_eq!(framework.name, "Flask");
        assert_eq!(framework.dev_command, "flask run --debug");
        assert_eq!(framework.port, Some(5000));

        let framework = detect_framework(&[path(&django)], &[]).unwrap();
        assert_eq!(framework.name, "Django");
        assert_eq!(framework.port, Some(8000));

        // Markers are checked in table order, so Flask wins over Django.
        let framework = detect_framework(&[path(&django), path(&flask)], &[]).unwrap();
        assert_eq!(framework.name, "Flask");

        assert!(detect_framework(&[path(&plain)], &[]).is_none());
        assert!(detect_framework(&[], &[]).is_none());
        // An unreadable requirements file is skipped rather than an error.
        assert!(detect_framework(&[path(&dir.path().join("missing.txt"))], &[]).is_none());
    }

    #[test]
    fn missing_dependencies_compare_names_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let requirements = dir.path().join("requirements.txt");
        write(
            &requirements,
            "# web\nFlask==3.0.2\n\nRequests>=2.31\nnumpy~=1.26\n  pandas  \nurllib3!=2.0.0\n",
        );
        let installed = [package("flask"), package("numpy"), package("urllib3")];

        let missing =
            find_missing_deps(Some(&requirements.to_string_lossy()), &installed).unwrap();
        assert_eq!(missing, vec!["requests", "pandas"]);

        assert!(find_missing_deps(None, &installed).unwrap().is_empty());
        assert!(
            find_missing_deps(Some(&dir.path().join("nope.txt").to_string_lossy()), &installed)
                .is_err()
        );
    }

    #[test]
    fn requirement_count_ignores_blanks_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let requirements = dir.path().join("requirements.txt");
        write(&requirements, "# pinned\nflask==3.0.2\n\n   \nrequests\n  # note\nnumpy\n");

        assert_eq!(count_requirements(Some(&requirements.to_string_lossy())).unwrap(), 3);
        assert_eq!(count_requirements(None).unwrap(), 0);
        assert!(count_requirements(Some(&dir.path().join("nope.txt").to_string_lossy())).is_err());
    }
}
