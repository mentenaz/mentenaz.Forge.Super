//! .NET / NuGet backend — SDK detection, project scanning, package
//! reference reading, `dotnet list package` JSON parsing, and the pure
//! parsers for NuGet's registry JSON shapes.
//!
//! No GPUI, no app-state dependency: every function here takes plain
//! strings/paths and returns a plain value or `Result` — a host panel wires
//! this into its own UI/state, this crate just does the work. The host panel
//! is responsible for making the HTTP calls to the NuGet registry; the
//! *parsing* of those responses lives here so it stays testable and shared.

use std::path::Path;

use serde::Serialize;

mod path_env;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ── Types ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct DotnetProject {
    /// Absolute path to the `.csproj` or `.sln` file itself.
    pub path: String,
    /// File stem (`MyApp`, `MyApp.sln` → `MyApp`).
    pub name: String,
    pub is_solution: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct InstalledPackage {
    pub id: String,
    pub version: String,
}

/// Which part of a package's version moved between the installed and latest
/// releases — drives the severity badge on the Updates page.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Serialize, Clone, Debug)]
pub struct OutdatedPackage {
    pub id: String,
    pub installed: String,
    pub latest: String,
    pub update_kind: UpdateKind,
}

#[derive(Serialize, Clone, Debug)]
pub struct VulnerablePackage {
    pub id: String,
    /// Normalized lowercase: "low" | "moderate" | "high" | "critical".
    pub severity: String,
    pub advisory_url: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct NugetSearchResult {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    pub total_downloads: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct NugetDependency {
    pub id: String,
    pub range: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct NugetDependencyGroup {
    pub target_framework: String,
    pub dependencies: Vec<NugetDependency>,
}

#[derive(Serialize, Clone, Debug)]
pub struct NugetPackageDetails {
    pub id: String,
    pub version: String,
    pub description: Option<String>,
    pub authors: String,
    pub project_url: Option<String>,
    pub license_url: Option<String>,
    /// Newest-first, full catalog order (capped by the UI, not here).
    pub versions: Vec<String>,
    /// `readme` is *not* resolved by the parser — the host fetches it from
    /// the v3-flatcontainer endpoint and hands it back in here.
    pub readme: Option<String>,
    pub dependencies: Vec<NugetDependencyGroup>,
    pub vulnerabilities: Vec<VulnerablePackage>,
}

// ── Filesystem ────────────────────────────────────────────────────────

fn read_dir_names(root: &Path) -> Vec<(String, bool)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| {
            let entry = e.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some((name, is_dir))
        })
        .collect()
}

// ── Project scanning ──────────────────────────────────────────────────

/// Maximum depth the .NET-project scanner descends from the workspace root.
pub const MAX_SCAN_DEPTH: usize = 5;
/// Directories never descended into while scanning for .NET projects.
pub const SCAN_SKIP_DIRS: &[&str] = &[
    ".git",
    ".vs",
    ".idea",
    ".cache",
    "node_modules",
    "bin",
    "obj",
    "target",
    "dist",
    "build",
    "out",
    "packages",
];

/// Walks `root` (to [`MAX_SCAN_DEPTH`], skipping [`SCAN_SKIP_DIRS`]) and
/// reports every `.csproj` / `.sln` file it finds. Stops descending once a
/// `.git` directory is found, treating that as a project boundary.
pub fn scan_dotnet_projects(root: &str, depth: usize) -> Vec<DotnetProject> {
    if depth > MAX_SCAN_DEPTH {
        return Vec::new();
    }
    let root = Path::new(root);
    let entries = read_dir_names(root);
    if entries.is_empty() {
        return Vec::new();
    }

    let mut projects = Vec::new();
    for (name, _) in &entries {
        if name.ends_with(".csproj") {
            let stem = name.trim_end_matches(".csproj").to_string();
            projects.push(DotnetProject {
                path: root.join(name).to_string_lossy().into_owned(),
                name: stem,
                is_solution: false,
            });
        } else if name.ends_with(".sln") {
            let stem = name.trim_end_matches(".sln").to_string();
            projects.push(DotnetProject {
                path: root.join(name).to_string_lossy().into_owned(),
                name: stem,
                is_solution: true,
            });
        }
    }

    if depth > 0
        && entries
            .iter()
            .any(|(name, is_dir)| name == ".git" && *is_dir)
    {
        return projects;
    }

    for (name, is_dir) in &entries {
        if !*is_dir {
            continue;
        }
        if SCAN_SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let sub_root = root.join(name);
        projects.extend(scan_dotnet_projects(&sub_root.to_string_lossy(), depth + 1));
    }

    projects
}

/// Runtime/package operations (install/update/remove, `dotnet list package`)
/// target a project file, never a solution — and the CLI itself rejects a
/// directory that contains more than one project. This resolves the csproj a
/// host should operate on:
/// - a `.csproj` path → itself;
/// - a `.sln` path → a csproj sitting next to it (else the nearest one);
/// - a bare directory → the nearest csproj within it.
/// Returns `None` when no csproj can be found.
pub fn resolve_csproj(project_path: &str) -> Option<String> {
    let p = Path::new(project_path);
    if p.extension()
        .map(|e| e.eq_ignore_ascii_case("csproj"))
        .unwrap_or(false)
    {
        return Some(project_path.to_string());
    }

    let dir = if p.is_dir() {
        p
    } else {
        p.parent().unwrap_or(p)
    };
    // A csproj directly next to the selected sln is the obvious favourite.
    if let Some((name, _)) = read_dir_names(dir)
        .into_iter()
        .find(|(name, _)| name.ends_with(".csproj"))
    {
        return Some(dir.join(name).to_string_lossy().into_owned());
    }

    let dir_str = dir.to_string_lossy().into_owned();
    scan_dotnet_projects(&dir_str, 2)
        .into_iter()
        .find(|proj| !proj.is_solution)
        .map(|proj| proj.path)
}

/// First `.csproj` under `root` (bounded scan), used by the manager when the
/// user hasn't picked a specific project yet.
pub fn find_csproj(root: &str) -> Option<String> {
    scan_dotnet_projects(root, 2)
        .into_iter()
        .find(|proj| !proj.is_solution)
        .map(|proj| proj.path)
}

// ── package reference parsing ─────────────────────────────────────────

/// Extract `(id, version)` pairs from `<PackageReference Include="X"
/// Version="Y" />` tags, attribute order independent. Handles both the
/// self-closing form and the child-element form
/// (`<PackageReference Include="X"><Version>Y</Version></PackageReference>`),
/// and skips closing tags and `PackageReferenceUpdate` (dotnet-tool style,
/// not a package reference).
pub fn parse_package_refs(xml: &str) -> Vec<InstalledPackage> {
    const KEY: &str = "PackageReference";
    const CLOSE: &str = "</PackageReference>";
    let bytes = xml.as_bytes();
    let mut out = Vec::new();
    let mut idx = 0;
    while idx < bytes.len() {
        let Some(rel) = xml[idx..].find(KEY) else {
            break;
        };
        let start = idx + rel;

        // Skip closing tags (`prev == '/'`).
        if start > 0 && bytes[start - 1] == b'/' {
            idx = start + KEY.len();
            continue;
        }
        // Skip longer identifiers like `PackageReferenceUpdate`.
        let prev_ok =
            start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
        let after_word = &xml[start + KEY.len()..];
        let next_ok = after_word
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        if !prev_ok || !next_ok {
            idx = start + KEY.len();
            continue;
        }

        let Some(gt) = after_word.find('>') else {
            break;
        };
        let attrs = &after_word[..gt];
        let self_closing = attrs.trim_end().ends_with('/');
        let id = attr_val(attrs, "Include");
        let mut version = attr_val(attrs, "Version");

        let mut next_idx = start + KEY.len() + gt + 1;
        if !self_closing && version.is_none() {
            // Child-element form: look for `<Version>…</Version>` inside the
            // element body before the matching `</PackageReference>`.
            if let Some(close) = after_word[gt + 1..].find(CLOSE) {
                let body = &after_word[gt + 1..gt + 1 + close];
                version = child_element(body, "Version");
                next_idx = start + KEY.len() + gt + 1 + close + CLOSE.len();
            }
        }

        if let (Some(id), Some(version)) = (id, version) {
            if !id.trim().is_empty() && !version.trim().is_empty() {
                out.push(InstalledPackage { id, version });
            }
        }
        idx = next_idx;
    }
    out
}

/// Extract `(id, version)` pairs from a legacy `packages.config`
/// (`<package id="X" version="Y" targetFramework="…" />`).
pub fn parse_packages_config(xml: &str) -> Vec<InstalledPackage> {
    const KEY: &str = "<package";
    let mut outer_idx = 0;
    let mut out = Vec::new();
    while outer_idx < xml.len() {
        let Some(rel) = xml[outer_idx..].find(KEY) else {
            break;
        };
        let start = outer_idx + rel;
        let after = &xml[start + KEY.len()..];
        let next_ok = after
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        if !next_ok {
            outer_idx = start + KEY.len();
            continue;
        }
        let Some(gt) = after.find('>') else {
            break;
        };
        let attrs = &after[..gt];
        if let (Some(id), Some(version)) = (attr_val(attrs, "id"), attr_val(attrs, "version")) {
            if !id.trim().is_empty() && !version.trim().is_empty() {
                out.push(InstalledPackage { id, version });
            }
        }
        outer_idx = start + KEY.len() + gt + 1;
    }
    out
}

/// Read an attribute out of a tag's attribute list, respecting either quote
/// style and any whitespace around the `=`.
fn attr_val(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(i) = rest.find(name) {
        rest = &rest[i..];
        if !rest.starts_with(&format!("{name}=")) {
            rest = &rest[name.len()..];
            continue;
        }
        let after_eq = rest[name.len() + 1..].trim_start();
        let quote = after_eq.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let body = &after_eq[1..];
        let end = body.find(quote)?;
        return Some(body[..end].to_string());
    }
    None
}

/// Extract the body of the first `<name>…</name>` element in `element`.
fn child_element(element: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let body = element.split(&open).nth(1)?.split(&close).next()?;
    let trimmed = body.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Read every `<PackageReference>` (csproj) plus a sibling `packages.config`
/// (legacy), deduped by `(id, version)` and sorted by id. Missing/unreadable
/// files yield an empty list rather than an error — a project with no
/// package refs is legitimately not an error.
pub fn read_installed_packages(csproj_path: &str) -> Vec<InstalledPackage> {
    let mut out: Vec<InstalledPackage> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(csproj_path) {
        out.extend(parse_package_refs(&content));
    }
    if let Some(dir) = Path::new(csproj_path).parent() {
        if let Ok(content) = std::fs::read_to_string(dir.join("packages.config")) {
            out.extend(parse_packages_config(&content));
        }
    }

    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert((p.id.to_lowercase(), p.version.clone())));
    out.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    out
}

// ── version helpers ───────────────────────────────────────────────────

/// Parse `major.minor.patch` from a semver-ish string, tolerating
/// 1- and 2-part versions and prerelease/build suffixes
/// (`1.2.3-beta.1` → `(1, 2, 3)`).
fn version_parts(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let major = it.next()?.trim().parse().ok()?;
    let minor = it
        .next()
        .map(|s| s.trim().parse().unwrap_or(0))
        .unwrap_or(0);
    let patch = it
        .next()
        .map(|s| {
            let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().unwrap_or(0)
        })
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Classify the kind of update from `installed` → `latest`. Unparseable or
/// equal versions default to `Patch` (the least alarming classification) —
/// `python_backend::classify_update` deliberately matches this fallback.
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

/// Human-friendly download count: `999 → "999"`, `1500 → "1.5K"`,
/// `2_400_000 → "2.4M"`, `3_100_000_000 → "3.1B"`.
pub fn fmt_downloads(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ── dotnet CLI ────────────────────────────────────────────────────────

/// Run `dotnet` synchronously with `args` in `cwd`, returning captured
/// stdout. Falls back to `cmd /C dotnet …` on Windows if the bare spawn
/// fails (odd PATH setups where only the `.bat` shim is resolvable), and
/// reports stderr when the CLI exits non-zero (which is how
/// `dotnet list package` surfaces "must restore first" / "unrecognized
/// project" style failures).
fn run_dotnet_captured(args: &[&str], cwd: &str) -> Result<String, String> {
    // `dotnet` evaluates the project's own build files, which can run programs,
    // so it only runs in a project that is trusted.
    forge_trust_gate::check_run(
        "dotnet_backend::run_dotnet_captured",
        "dotnet",
        args,
        Some(std::path::Path::new(cwd)),
    )?;
    run_dotnet_unchecked(args, cwd)
}

/// [`run_dotnet_captured`] without the trust check, for `dotnet --version`, which
/// runs in the process's own folder and reads no project.
fn run_dotnet_unchecked(args: &[&str], cwd: &str) -> Result<String, String> {
    let mut c = gpui_util::new_std_command("dotnet");
    c.args(args).current_dir(cwd);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
        path_env::enrich_path(&mut c);
    }
    let out = match c.output() {
        Ok(out) => out,
        Err(e) => {
            log::debug!("run_dotnet_captured: direct 'dotnet' failed: {e}");
            #[cfg(target_os = "windows")]
            {
                let mut sh = gpui_util::new_std_command("cmd");
                sh.args(["/C", "dotnet"]).args(args).current_dir(cwd);
                use std::os::windows::process::CommandExt;
                sh.creation_flags(CREATE_NO_WINDOW);
                path_env::enrich_path(&mut sh);
                match sh.output() {
                    Ok(fb) => {
                        log::debug!(
                            "run_dotnet_captured: cmd /C 'dotnet' status={} stdout={} stderr={}",
                            fb.status,
                            String::from_utf8_lossy(&fb.stdout).trim(),
                            String::from_utf8_lossy(&fb.stderr).trim(),
                        );
                        fb
                    }
                    Err(e2) => {
                        log::debug!("run_dotnet_captured: cmd /C 'dotnet' also failed: {e2}");
                        return Err("`dotnet` was not found on PATH".to_string());
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            return Err("`dotnet` was not found on PATH".to_string());
        }
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("dotnet exited with code {:?}", out.status.code())
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// [`Self::run_dotnet_captured`] wrapper for the manager's background-run
/// fallback (panel code holds its arguments as `Vec<String>` instead of
/// string literals).
pub fn run_dotnet_args(args: &[String], cwd: &str) -> Result<String, String> {
    let strs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_dotnet_captured(&strs, cwd)
}

/// Query `dotnet --version`.
pub fn query_runtime(_exe: String) -> Result<String, String> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    run_dotnet_unchecked(&["--version"], &cwd)
}

/// Outdated packages (`dotnet list package --outdated --format json`), run
/// in the *folder containing* the solution/underlying csproj — the CLI
/// refuses directories that hold more than one project file. Blank output
/// (no projects / no refs) is a success, not an error.
pub fn list_outdated(dir: String) -> Result<Vec<OutdatedPackage>, String> {
    let stdout = run_dotnet_captured(&["list", "package", "--outdated", "--format", "json"], &dir)?;
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_outdated_json(&stdout)
}

/// Vulnerable packages (`dotnet list package --vulnerable --format json`).
pub fn list_vulnerable(dir: String) -> Result<Vec<VulnerablePackage>, String> {
    let stdout = run_dotnet_captured(
        &["list", "package", "--vulnerable", "--format", "json"],
        &dir,
    )?;
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_vulnerable_json(&stdout)
}

/// Parse `dotnet list package --outdated --format json` (verified against
/// .NET 8/9/10 SDK output): `projects[].frameworks[].topLevelPackages[]`
/// with `resolvedVersion` / `latestVersion` per entry. Duplicate ids across
/// frameworks are collapsed (first occurrence wins, sorted).
pub fn parse_outdated_json(json: &str) -> Result<Vec<OutdatedPackage>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("Failed to parse dotnet list package output: {e}"))?;

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();
    for project in value
        .get("projects")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        for fw in project
            .get("frameworks")
            .and_then(|f| f.as_array())
            .into_iter()
            .flatten()
        {
            for pkg in fw
                .get("topLevelPackages")
                .and_then(|p| p.as_array())
                .into_iter()
                .flatten()
            {
                let id = pkg
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() || !seen.insert(id.to_lowercase()) {
                    continue;
                }
                let installed = pkg
                    .get("resolvedVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let latest = pkg
                    .get("latestVersion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if installed.is_empty() || latest.is_empty() || installed == latest {
                    continue;
                }
                entries.push(OutdatedPackage {
                    update_kind: classify_update(&installed, &latest),
                    id,
                    installed,
                    latest,
                });
            }
        }
    }
    entries.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    Ok(entries)
}

/// Parse `dotnet list package --vulnerable --format json`. The SDK emits
/// Title-Case severity ("Low"/"Moderate"/…) and a lowercase `advisoryurl`
/// field — both normalized here so the panel can treat it like the audit
/// data it renders alongside.
pub fn parse_vulnerable_json(json: &str) -> Result<Vec<VulnerablePackage>, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| format!("Failed to parse dotnet list package output: {e}"))?;

    let mut out = Vec::new();
    for project in value
        .get("projects")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        for fw in project
            .get("frameworks")
            .and_then(|f| f.as_array())
            .into_iter()
            .flatten()
        {
            for pkg in fw
                .get("topLevelPackages")
                .and_then(|p| p.as_array())
                .into_iter()
                .flatten()
            {
                let id = pkg
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                for vuln in pkg
                    .get("vulnerabilities")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    let severity = vuln
                        .get("severity")
                        .and_then(|s| s.as_str())
                        .unwrap_or("low")
                        .to_lowercase();
                    let advisory_url = vuln
                        .get("advisoryurl")
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(VulnerablePackage {
                        id: id.clone(),
                        severity,
                        advisory_url,
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    Ok(out)
}

// ── NuGet registry parsing (pure) ─────────────────────────────────────
//
// The host performs the GETs; these functions turn the JSON bodies into the
// panel-friendly shapes. Endpoints verified against the real service index
// at https://api.nuget.org/v3/index.json.

/// Registration index URL for `id` (lowercased — registry is case-insensitive
/// but the CDN paths are not). Uses `registration5-semver1` (plain JSON,
/// prereleases) rather than the bare `registration5` path, which 404s.
pub fn nuget_registration_url(id: &str) -> String {
    format!(
        "https://api.nuget.org/v3/registration5-semver1/{}/index.json",
        id.to_lowercase()
    )
}

/// Raw README URL for a package/version via the v3-flatcontainer — always
/// returns markdown/text, never nuget.org's HTML gallery page (the
/// catalog's own `readmeUrl` can point at that, which renders terribly).
pub fn nuget_flat_readme_url(id: &str, version: &str) -> String {
    format!(
        "https://api.nuget.org/v3-flatcontainer/{}/{}/readme",
        id.to_lowercase(),
        version.to_lowercase()
    )
}

/// Parse the Azure-hosted search response
/// (`https://azuresearch-usnc.nuget.org/query`): `totalHits` + `data[]`.
pub fn parse_nuget_search(
    json: &serde_json::Value,
) -> Result<(Vec<NugetSearchResult>, u32), String> {
    let total_hits = json.get("totalHits").and_then(|v| v.as_u64()).unwrap_or(0);
    let data = json
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut results = Vec::new();
    for d in data {
        let id = d
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        results.push(NugetSearchResult {
            version: d
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            description: d
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from),
            total_downloads: d
                .get("totalDownloads")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            id,
        });
    }
    Ok((results, total_hits as u32))
}

/// Flatten the `items` of every registration page (the host fetches each
/// page's `@id` when the page has `items` missing / empty — that's the
/// "too many versions to inline" case). Registration pages are ordered
/// oldest-to-newest.
pub fn registration_items_from_pages(pages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    pages
        .iter()
        .flat_map(|p| {
            p.get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

/// Build [`NugetPackageDetails`] from the flattened catalog (`items`) of a
/// registration — newest-first versions, plus latest-version metadata. The
/// `readme` field is left `None` for the host to fill in (there is no way
/// to know without an extra HTTP request whether the version has one).
pub fn parse_nuget_details(catalog: &[serde_json::Value]) -> Result<NugetPackageDetails, String> {
    let versions: Vec<String> = catalog
        .iter()
        .rev()
        .filter_map(|e| {
            e.get("catalogEntry")?
                .get("version")?
                .as_str()
                .map(String::from)
        })
        .collect();

    let latest_entry = catalog
        .last()
        .and_then(|e| e.get("catalogEntry"))
        .ok_or_else(|| "No version metadata found for this package".to_string())?;

    let id = latest_entry
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let version = latest_entry
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() || version.is_empty() {
        return Err("Empty package metadata".to_string());
    }

    let dependencies = latest_entry
        .get("dependencyGroups")
        .and_then(|v| v.as_array())
        .map(|groups| {
            groups
                .iter()
                .map(|g| {
                    let target_framework = g
                        .get("targetFramework")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let deps = g
                        .get("dependencies")
                        .and_then(|v| v.as_array())
                        .map(|deps| {
                            deps.iter()
                                .filter_map(|d| {
                                    let dep_id = d.get("id")?.as_str()?.to_string();
                                    Some(NugetDependency {
                                        range: d
                                            .get("range")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        id: dep_id,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    NugetDependencyGroup {
                        target_framework,
                        dependencies: deps,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let vulnerabilities = latest_entry
        .get("vulnerabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    Some(VulnerablePackage {
                        id: v
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or(&id)
                            .to_string(),
                        severity: v.get("severity")?.as_str()?.to_lowercase(),
                        advisory_url: v
                            .get("advisoryUrl")
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let authors = normalize_authors(latest_entry.get("authors"));

    Ok(NugetPackageDetails {
        version,
        id,
        description: latest_entry
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from),
        authors,
        project_url: latest_entry
            .get("projectUrl")
            .and_then(|v| v.as_str())
            .map(String::from),
        license_url: latest_entry
            .get("licenseUrl")
            .and_then(|v| v.as_str())
            .map(String::from),
        versions,
        readme: None,
        dependencies,
        vulnerabilities,
    })
}

/// `authors` is a string on most registration entries, but some packages
/// publish it as a JSON array — normalize both to a single comma-joined
/// string.
fn normalize_authors(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pkg(id: &str, version: &str) -> InstalledPackage {
        InstalledPackage {
            id: id.to_string(),
            version: version.to_string(),
        }
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "").unwrap();
    }

    fn sorted_names(projects: &[DotnetProject]) -> Vec<String> {
        let mut names: Vec<String> = projects.iter().map(|p| p.name.clone()).collect();
        names.sort();
        names
    }

    #[test]
    fn package_refs_self_closing_in_either_attribute_order() {
        let xml = r#"
            <ItemGroup>
              <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
              <PackageReference Version="8.0.0" Include="Serilog" />
              <PackageReference Include='Dapper' Version='2.1.35'/>
            </ItemGroup>"#;
        assert_eq!(
            parse_package_refs(xml),
            vec![
                pkg("Newtonsoft.Json", "13.0.3"),
                pkg("Serilog", "8.0.0"),
                pkg("Dapper", "2.1.35"),
            ]
        );
    }

    #[test]
    fn package_refs_child_element_version() {
        let xml = r#"
            <PackageReference Include="Polly">
              <Version>8.4.1</Version>
              <PrivateAssets>all</PrivateAssets>
            </PackageReference>
            <PackageReference Include="xunit" Version="2.9.0" />"#;
        assert_eq!(
            parse_package_refs(xml),
            vec![pkg("Polly", "8.4.1"), pkg("xunit", "2.9.0")]
        );
    }

    #[test]
    fn package_refs_skip_versionless_and_longer_identifiers() {
        let xml = r#"
            <PackageReference Include="CentrallyManaged" />
            <PackageReferenceUpdate Include="NotAReference" Version="1.0.0" />
            <PackageReference Include="Kept" Version="1.2.3" />"#;
        assert_eq!(parse_package_refs(xml), vec![pkg("Kept", "1.2.3")]);
    }

    #[test]
    fn package_refs_empty_input() {
        assert!(parse_package_refs("").is_empty());
        assert!(parse_package_refs("<Project></Project>").is_empty());
    }

    #[test]
    fn packages_config_ignores_the_root_element() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
            <packages>
              <package id="EntityFramework" version="6.4.4" targetFramework="net48" />
              <package version="4.7.2" id="NUnit" />
              <package id="NoVersion" />
            </packages>"#;
        assert_eq!(
            parse_packages_config(xml),
            vec![pkg("EntityFramework", "6.4.4"), pkg("NUnit", "4.7.2")]
        );
    }

    #[test]
    fn installed_packages_merge_dedupe_and_sort() {
        let dir = tempfile::tempdir().unwrap();
        let csproj = dir.path().join("App.csproj");
        std::fs::write(
            &csproj,
            r#"<Project>
                 <PackageReference Include="Zeta" Version="1.0.0" />
                 <PackageReference Include="alpha" Version="2.0.0" />
               </Project>"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("packages.config"),
            r#"<packages>
                 <package id="ALPHA" version="2.0.0" />
                 <package id="Mid" version="3.0.0" />
               </packages>"#,
        )
        .unwrap();

        let installed = read_installed_packages(&csproj.to_string_lossy());
        assert_eq!(
            installed,
            vec![pkg("alpha", "2.0.0"), pkg("Mid", "3.0.0"), pkg("Zeta", "1.0.0")]
        );
    }

    #[test]
    fn installed_packages_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("Nope.csproj");
        assert!(read_installed_packages(&missing.to_string_lossy()).is_empty());
    }

    #[test]
    fn classify_update_by_changed_component() {
        assert_eq!(classify_update("1.2.3", "2.0.0"), UpdateKind::Major);
        assert_eq!(classify_update("1.2.3", "1.3.0"), UpdateKind::Minor);
        assert_eq!(classify_update("1.2.3", "1.2.4"), UpdateKind::Patch);
        // Short versions pad with zeros; prerelease suffixes are ignored.
        assert_eq!(classify_update("1.2", "1.3"), UpdateKind::Minor);
        assert_eq!(classify_update("1.2.3-beta.1", "1.2.4"), UpdateKind::Patch);
    }

    #[test]
    fn classify_update_falls_back_to_patch() {
        assert_eq!(classify_update("1.2.3", "1.2.3"), UpdateKind::Patch);
        assert_eq!(classify_update("not-a-version", "2.0.0"), UpdateKind::Patch);
        assert_eq!(classify_update("1.0.0", ""), UpdateKind::Patch);
    }

    #[test]
    fn update_kind_labels() {
        assert_eq!(UpdateKind::Major.label(), "major");
        assert_eq!(UpdateKind::Minor.label(), "minor");
        assert_eq!(UpdateKind::Patch.label(), "patch");
    }

    #[test]
    fn download_counts_are_abbreviated() {
        assert_eq!(fmt_downloads(0), "0");
        assert_eq!(fmt_downloads(999), "999");
        assert_eq!(fmt_downloads(1_500), "1.5K");
        assert_eq!(fmt_downloads(2_400_000), "2.4M");
        assert_eq!(fmt_downloads(3_100_000_000), "3.1B");
    }

    #[test]
    fn outdated_json_dedupes_across_frameworks_and_sorts() {
        let json = json!({
            "projects": [{
                "frameworks": [
                    {
                        "framework": "net8.0",
                        "topLevelPackages": [
                            { "id": "Serilog", "resolvedVersion": "3.0.0", "latestVersion": "4.0.0" },
                            { "id": "Dapper", "resolvedVersion": "2.1.0", "latestVersion": "2.1.35" },
                            { "id": "UpToDate", "resolvedVersion": "1.0.0", "latestVersion": "1.0.0" },
                            { "id": "NoLatest", "resolvedVersion": "1.0.0" }
                        ]
                    },
                    {
                        "framework": "net9.0",
                        "topLevelPackages": [
                            { "id": "serilog", "resolvedVersion": "3.1.0", "latestVersion": "4.0.0" }
                        ]
                    }
                ]
            }]
        })
        .to_string();

        let outdated = parse_outdated_json(&json).unwrap();
        let summary: Vec<_> = outdated
            .iter()
            .map(|p| (p.id.as_str(), p.installed.as_str(), p.latest.as_str(), p.update_kind))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Dapper", "2.1.0", "2.1.35", UpdateKind::Patch),
                ("Serilog", "3.0.0", "4.0.0", UpdateKind::Major),
            ]
        );
    }

    #[test]
    fn outdated_json_tolerates_missing_sections_but_not_bad_json() {
        assert!(parse_outdated_json("{}").unwrap().is_empty());
        assert!(parse_outdated_json(r#"{"projects":[{}]}"#).unwrap().is_empty());
        assert!(parse_outdated_json("not json").is_err());
    }

    #[test]
    fn vulnerable_json_normalizes_severity_and_url() {
        let json = json!({
            "projects": [{
                "frameworks": [{
                    "topLevelPackages": [
                        {
                            "id": "Zed.Pkg",
                            "vulnerabilities": [
                                { "severity": "High", "advisoryurl": "https://example.test/a" },
                                { "advisoryurl": "https://example.test/b" }
                            ]
                        },
                        { "id": "Clean.Pkg" },
                        {
                            "id": "Alpha.Pkg",
                            "vulnerabilities": [{ "severity": "CRITICAL" }]
                        }
                    ]
                }]
            }]
        })
        .to_string();

        let vulnerable = parse_vulnerable_json(&json).unwrap();
        let summary: Vec<_> = vulnerable
            .iter()
            .map(|v| (v.id.as_str(), v.severity.as_str(), v.advisory_url.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Alpha.Pkg", "critical", ""),
                ("Zed.Pkg", "high", "https://example.test/a"),
                ("Zed.Pkg", "low", "https://example.test/b"),
            ]
        );
        assert!(parse_vulnerable_json("[").is_err());
    }

    #[test]
    fn registry_urls_are_lowercased() {
        assert_eq!(
            nuget_registration_url("Newtonsoft.Json"),
            "https://api.nuget.org/v3/registration5-semver1/newtonsoft.json/index.json"
        );
        assert_eq!(
            nuget_flat_readme_url("Serilog", "4.0.0-Beta"),
            "https://api.nuget.org/v3-flatcontainer/serilog/4.0.0-beta/readme"
        );
    }

    #[test]
    fn search_results_skip_entries_without_an_id() {
        let json = json!({
            "totalHits": 42,
            "data": [
                { "id": "Serilog", "version": "4.0.0", "description": "Logging", "totalDownloads": 1500 },
                { "version": "1.0.0" },
                { "id": "Bare" }
            ]
        });
        let (results, total) = parse_nuget_search(&json).unwrap();
        assert_eq!(total, 42);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "Serilog");
        assert_eq!(results[0].version, "4.0.0");
        assert_eq!(results[0].description.as_deref(), Some("Logging"));
        assert_eq!(results[0].total_downloads, 1500);
        assert_eq!(results[1].id, "Bare");
        assert_eq!(results[1].version, "");
        assert_eq!(results[1].description, None);
        assert_eq!(results[1].total_downloads, 0);

        let (results, total) = parse_nuget_search(&json!({})).unwrap();
        assert!(results.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn registration_pages_flatten_in_order() {
        let pages = vec![
            json!({ "items": [{ "n": 1 }, { "n": 2 }] }),
            json!({ "count": 64 }),
            json!({ "items": [{ "n": 3 }] }),
        ];
        let items = registration_items_from_pages(&pages);
        let order: Vec<_> = items.iter().map(|i| i["n"].as_u64().unwrap()).collect();
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[test]
    fn details_use_the_newest_entry_and_list_versions_newest_first() {
        let catalog = vec![
            json!({ "catalogEntry": { "id": "Serilog", "version": "3.0.0" } }),
            json!({ "catalogEntry": {
                "id": "Serilog",
                "version": "4.0.0",
                "description": "Simple .NET logging",
                "dependencyGroups": [{
                    "targetFramework": "net8.0",
                    "dependencies": [
                        { "id": "System.Text.Json", "range": "[8.0.0, )" },
                        { "range": "[1.0.0, )" }
                    ]
                }],
                "vulnerabilities": [
                    { "severity": "Moderate", "advisoryUrl": "https://example.test/adv" }
                ]
            } }),
        ];

        let details = parse_nuget_details(&catalog).unwrap();
        assert_eq!(details.id, "Serilog");
        assert_eq!(details.version, "4.0.0");
        assert_eq!(details.description.as_deref(), Some("Simple .NET logging"));
        assert_eq!(details.versions, vec!["4.0.0", "3.0.0"]);
        assert_eq!(details.readme, None);

        assert_eq!(details.dependencies.len(), 1);
        assert_eq!(details.dependencies[0].target_framework, "net8.0");
        assert_eq!(details.dependencies[0].dependencies.len(), 1);
        assert_eq!(details.dependencies[0].dependencies[0].id, "System.Text.Json");
        assert_eq!(details.dependencies[0].dependencies[0].range, "[8.0.0, )");

        assert_eq!(details.vulnerabilities.len(), 1);
        assert_eq!(details.vulnerabilities[0].id, "Serilog");
        assert_eq!(details.vulnerabilities[0].severity, "moderate");
        assert_eq!(details.vulnerabilities[0].advisory_url, "https://example.test/adv");
    }

    #[test]
    fn details_reject_empty_or_blank_catalogs() {
        assert!(parse_nuget_details(&[]).is_err());
        assert!(parse_nuget_details(&[json!({ "catalogEntry": { "id": "", "version": "1.0.0" } })]).is_err());
        assert!(parse_nuget_details(&[json!({ "noEntry": true })]).is_err());
    }

    #[test]
    fn scan_finds_projects_and_solutions_but_skips_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Everything.sln"));
        touch(&root.join("src/App/App.csproj"));
        touch(&root.join("src/Lib/Lib.csproj"));
        touch(&root.join("src/App/bin/Debug/Stale.csproj"));
        touch(&root.join("src/App/obj/Generated.csproj"));
        touch(&root.join("node_modules/pkg/Vendored.csproj"));

        let projects = scan_dotnet_projects(&root.to_string_lossy(), 0);
        assert_eq!(sorted_names(&projects), vec!["App", "Everything", "Lib"]);

        let solution = projects.iter().find(|p| p.name == "Everything").unwrap();
        assert!(solution.is_solution);
        assert!(solution.path.ends_with("Everything.sln"));
        assert!(projects.iter().filter(|p| p.name != "Everything").all(|p| !p.is_solution));
    }

    #[test]
    fn scan_treats_a_nested_git_directory_as_a_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        touch(&root.join("Root.csproj"));
        std::fs::create_dir_all(root.join("vendor/.git")).unwrap();
        touch(&root.join("vendor/Vendor.csproj"));
        touch(&root.join("vendor/deep/Deep.csproj"));

        // The root's own `.git` doesn't stop the scan; a nested repository's
        // does, after reporting the projects sitting directly in it.
        let projects = scan_dotnet_projects(&root.to_string_lossy(), 0);
        assert_eq!(sorted_names(&projects), vec!["Root", "Vendor"]);
    }

    #[test]
    fn scan_stops_past_the_depth_limit_and_on_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("App.csproj"));
        let root = dir.path().to_string_lossy().into_owned();

        assert_eq!(scan_dotnet_projects(&root, MAX_SCAN_DEPTH).len(), 1);
        assert!(scan_dotnet_projects(&root, MAX_SCAN_DEPTH + 1).is_empty());
        assert!(scan_dotnet_projects(&dir.path().join("missing").to_string_lossy(), 0).is_empty());
    }

    #[test]
    fn find_csproj_ignores_solutions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(&root.join("Everything.sln"));
        assert_eq!(find_csproj(&root.to_string_lossy()), None);

        touch(&root.join("src/App/App.csproj"));
        let found = find_csproj(&root.to_string_lossy()).unwrap();
        assert!(found.ends_with("App.csproj"));
    }

    #[test]
    fn resolve_csproj_from_project_solution_or_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let csproj = root.join("App.csproj");
        let sln = root.join("App.sln");
        touch(&csproj);
        touch(&sln);

        // A csproj path resolves to itself without touching the filesystem.
        assert_eq!(resolve_csproj("C:/nowhere/Thing.csproj").as_deref(), Some("C:/nowhere/Thing.csproj"));
        // A solution resolves to the project sitting next to it.
        assert_eq!(
            resolve_csproj(&sln.to_string_lossy()).as_deref(),
            Some(&*csproj.to_string_lossy())
        );
        // A directory resolves to the project inside it.
        assert_eq!(
            resolve_csproj(&root.to_string_lossy()).as_deref(),
            Some(&*csproj.to_string_lossy())
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(resolve_csproj(&empty.path().to_string_lossy()), None);
    }
}
