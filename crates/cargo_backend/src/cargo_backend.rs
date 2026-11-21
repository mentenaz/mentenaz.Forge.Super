//! Rust/Cargo backend — which crates a workspace has, what each depends on,
//! and what those dependencies are locked to.
//!
//! No GPUI, no app-state dependency: every function here takes plain
//! strings/paths and returns a plain value or `Result` — a host panel wires
//! this into its own UI/state, this crate just does the work.
//!
//! Nothing here involves rust-analyzer. A Rust project is recognized by its
//! `Cargo.toml` on disk, and everything is read from Cargo's own files and
//! one cheap Cargo command, so the manager works whether or not a language
//! server is running. See `docs/Rust_Manager_Design_Note.md`, section 3.
//!
//! Two sources, deliberately different:
//! - *What a crate declares* comes from `cargo metadata --no-deps`
//!   ([`load_workspace`]), because Cargo applies `workspace = true`
//!   inheritance for us and `--no-deps` stops it resolving the graph.
//! - *What is installed* comes from reading `Cargo.lock` directly
//!   ([`read_lockfile`]), because the resolved answer is already on disk.
//!
//! A third answers *what else exists*: the crates.io sparse index
//! ([`index_url`], [`parse_index`]), compared against the two above by
//! [`version_status`]. This crate computes the URL and reads the answer; the
//! request itself is the host's to make.
//!
//! And a fourth, *what is known to be wrong with it*: OSV.dev, asked about
//! every crates.io package reachable from the selected crate
//! ([`osv_batches`], [`parse_advisory`], [`merge_findings`]). Again the
//! requests are the host's.
//!
//! Changing things is the last part: the `cargo add`, `cargo remove` and
//! `cargo update` command lines ([`add_args`], [`remove_args`],
//! [`update_args`]) and what each does to the manifests beyond the obvious
//! ([`add_effect`], [`declaration`], [`remove_effect`]). The host runs the
//! commands; the only one run here is the dry run ([`update_dry_run`]).
//!
//! Finding something to add is the crates.io Web API ([`search_url`],
//! [`parse_search`]), and a crate's README is one more URL ([`readme_url`]).

use std::path::Path;

mod actions;
mod advisories;
mod index;
mod lockfile;
mod metadata;
mod outdated;
mod path_env;
mod search;

pub use actions::{
    AddEffect, Declaration, LockChange, LockChangeKind, Manifest, RemoveEffect, UpdateSpec,
    add_args, add_effect, check_crate_name, check_exact_version, command_line, declaration,
    parse_manifest, parse_update_output, read_crate_manifest, read_manifest,
    read_member_manifests, read_root_manifest, remove_args, remove_effect, update_args,
    update_dry_run,
};
pub use advisories::{
    AdvisoryRecord, AdvisoryRef, AffectedPackage, Finding, FindingCounts, FindingKind,
    OSV_BATCH_LIMIT, OSV_BATCH_URL, OSV_ECOSYSTEM, OsvBatch, PackageAdvisories, Severity,
    advisory_ids, advisory_page_url, advisory_url, cvss3_base_tenths, merge_findings,
    osv_batches, parse_advisory,
};
pub use index::{INDEX_BASE_URL, IndexVersion, index_path, index_url, parse_index};
pub use lockfile::{LockedDependency, LockedPackage, Lockfile, parse_lockfile};
pub use metadata::{
    CrateInfo, Dependency, DependencyKind, DependencySource, Workspace, parse_metadata,
};
pub use outdated::{
    RustCompat, UpdateKind, UpdateTarget, VersionChoice, VersionStatus, available_versions,
    classify_update, rust_compat, version_status,
};
pub use search::{
    SEARCH_BASE_URL, SEARCH_PAGE_SIZE, SearchPage, SearchResult, fmt_count, next_search_url,
    parse_search, readme_url, search_url,
};
pub use semver::Version;

pub const MANIFEST_FILE: &str = "Cargo.toml";
pub const LOCK_FILE: &str = "Cargo.lock";

/// Whether `dir` directly contains a `Cargo.toml` — how a Rust project is
/// detected, the way `package.json` marks a Node one.
pub fn is_cargo_project(dir: &str) -> bool {
    Path::new(dir).join(MANIFEST_FILE).is_file()
}

/// Runs `cargo` with `args`, returning its stdout. On failure the error is
/// Cargo's own stderr, which is already written for a person to read.
///
/// Blocks until Cargo exits, which is the contract of this crate's public
/// functions that call it: they are documented as blocking and are meant to
/// be run on a background task, like the other fork backends.
fn run_cargo(args: &[&str], cwd: Option<&str>) -> Result<String, String> {
    run_cargo_output(args, cwd).map(|(stdout, _)| stdout)
}

/// [`run_cargo`], also returning stderr from a successful run: Cargo
/// reports what it did there, not on stdout.
#[allow(clippy::disallowed_methods)]
fn run_cargo_output(args: &[&str], cwd: Option<&str>) -> Result<(String, String), String> {
    // `cargo` reads the project's own configuration (`.cargo/config.toml` can name
    // a compiler wrapper), so it only runs in a project that is trusted.
    if let Some(cwd) = cwd {
        forge_trust_gate::check_run(
            "cargo_backend::run_cargo_output",
            "cargo",
            args,
            Some(Path::new(cwd)),
        )?;
    }
    let mut command = gpui_util::new_std_command("cargo");
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    path_env::enrich_path(&mut command);

    let output = command
        .output()
        .map_err(|error| format!("Could not run cargo: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        return Ok((
            String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr.into_owned(),
        ));
    }
    let stderr = stderr.trim().to_string();
    Err(if stderr.is_empty() {
        format!("cargo {} failed ({})", args.join(" "), output.status)
    } else {
        stderr
    })
}

/// The `cargo metadata` call: manifests only, and the output format asked for by
/// number so a newer Cargo cannot change the shape under us.
const METADATA_ARGS: [&str; 4] = ["metadata", "--no-deps", "--format-version", "1"];

/// Loads the workspace that `dir` belongs to: its root, its crates, and each
/// crate's declared dependencies.
///
/// Runs `cargo metadata --no-deps`, which reads manifests only — it does not
/// resolve dependencies, touch the network, or compile. On a 286-crate
/// workspace it takes about a second. Blocking: call from a background task.
pub fn load_workspace(dir: &str) -> Result<Workspace, String> {
    let json = run_cargo(&METADATA_ARGS, Some(dir))?;
    parse_metadata(&json)
}

/// Reads and parses the `Cargo.lock` in `workspace_root`.
///
/// A missing lockfile is an error here rather than an empty result: callers
/// distinguish "nothing locked yet" (show dependencies without installed
/// versions) from "could not read it" by whether the file exists.
pub fn read_lockfile(workspace_root: &str) -> Result<Lockfile, String> {
    let path = Path::new(workspace_root).join(LOCK_FILE);
    let contents = std::fs::read_to_string(&path)
        .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
    parse_lockfile(&contents)
}

/// Whether `workspace_root` has a `Cargo.lock` at all. A library crate that
/// has never been built legitimately doesn't.
pub fn has_lockfile(workspace_root: &str) -> bool {
    Path::new(workspace_root).join(LOCK_FILE).is_file()
}

// ── Toolchain ──────────────────────────────────────────────────────────

/// The installed Cargo version, e.g. `1.98.1`. Blocking, like
/// [`load_workspace`].
pub fn query_cargo() -> Result<String, String> {
    let output = run_cargo(&["--version"], None)?;
    version_from_output(&output).ok_or_else(|| format!("Unexpected cargo output: {output}"))
}

/// The installed `rustc` version, e.g. `1.98.1` — what a dependency's
/// declared minimum Rust version is compared against. Blocking, like
/// [`load_workspace`].
#[allow(clippy::disallowed_methods)]
pub fn query_rustc() -> Result<String, String> {
    let mut command = gpui_util::new_std_command("rustc");
    command.arg("--version");
    path_env::enrich_path(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Could not run rustc: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    version_from_output(&stdout).ok_or_else(|| format!("Unexpected rustc output: {stdout}"))
}

/// Pulls the version number out of `rustc --version` / `cargo --version`
/// output (`rustc 1.98.1 (01f6ddf75 2026-08-05)` → `1.98.1`): the first
/// whitespace-separated token that starts with a digit.
pub fn version_from_output(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
}

// ── Dependency list ────────────────────────────────────────────────────

/// One row of a crate's dependency list: what it declares, joined with what
/// the lockfile resolved it to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedDependency {
    /// The package name on crates.io.
    pub name: String,
    /// The name it is imported under, when renamed.
    pub rename: Option<String>,
    /// The declared version requirement (`^1.0`).
    pub requirement: String,
    pub kind: DependencyKind,
    /// The `cfg(...)`/target this dependency is limited to, if any.
    pub target: Option<String>,
    pub optional: bool,
    /// The exact version in `Cargo.lock`. `None` when there is no lockfile,
    /// or it predates this dependency — shown as unknown, never guessed.
    pub locked_version: Option<String>,
}

/// A crate's direct dependencies, as the manager shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DependencyList {
    /// crates.io dependencies, sorted by name then kind.
    pub listed: Vec<ListedDependency>,
    /// How many distinct path, git and other-registry dependencies were left
    /// out. Shown as a single line ("12 local and git dependencies not
    /// shown") so the list never looks complete when it isn't.
    pub hidden: usize,
}

impl DependencyList {
    /// The distinct package names to look up in the registry index, in list
    /// order. A package declared twice (normal and dev, or under two
    /// targets) needs only one request.
    pub fn registry_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for row in &self.listed {
            if !names.contains(&row.name.as_str()) {
                names.push(&row.name);
            }
        }
        names
    }
}

impl ListedDependency {
    /// This row's standing against the registry, given the package's index
    /// entries and the installed toolchain version.
    pub fn status(&self, versions: &[IndexVersion], toolchain: Option<&str>) -> VersionStatus {
        version_status(
            &self.requirement,
            self.locked_version.as_deref(),
            versions,
            toolchain,
        )
    }
}

/// The dependency list for `krate`: its crates.io dependencies with their
/// locked versions, plus a count of the ones not listed.
///
/// Pass `None` for the lockfile when there isn't one; rows then simply have
/// no locked version.
pub fn direct_dependencies(krate: &CrateInfo, lockfile: Option<&Lockfile>) -> DependencyList {
    let listed = krate
        .dependencies
        .iter()
        .filter(|dependency| dependency.is_listed())
        .map(|dependency| ListedDependency {
            name: dependency.name.clone(),
            rename: dependency.rename.clone(),
            requirement: dependency.requirement.clone(),
            kind: dependency.kind,
            target: dependency.target.clone(),
            optional: dependency.optional,
            locked_version: lockfile
                .and_then(|lockfile| {
                    lockfile.locked_version(&krate.name, &krate.version, &dependency.name)
                })
                .map(str::to_string),
        })
        .collect();

    // A dependency declared in two tables (normal and dev, say) is still one
    // thing the user isn't being shown.
    let mut hidden: Vec<&str> = krate
        .dependencies
        .iter()
        .filter(|dependency| !dependency.is_listed())
        .map(|dependency| dependency.name.as_str())
        .collect();
    hidden.sort_unstable();
    hidden.dedup();

    DependencyList {
        listed,
        hidden: hidden.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

    #[test]
    fn cargo_metadata_is_always_asked_for_manifests_only_in_format_version_1() {
        assert_eq!(
            METADATA_ARGS,
            ["metadata", "--no-deps", "--format-version", "1"]
        );
    }

    fn dependency(name: &str, kind: DependencyKind, source: DependencySource) -> Dependency {
        Dependency {
            name: name.to_string(),
            rename: None,
            requirement: "^1".to_string(),
            kind,
            target: None,
            source,
            optional: false,
            uses_default_features: true,
            features: Vec::new(),
        }
    }

    fn app(dependencies: Vec<Dependency>) -> CrateInfo {
        CrateInfo {
            name: "app".to_string(),
            version: "1.0.0".to_string(),
            manifest_path: "/work/app/Cargo.toml".to_string(),
            rust_version: None,
            dependencies,
        }
    }

    fn lockfile() -> Lockfile {
        parse_lockfile(&format!(
            r#"
[[package]]
name = "app"
version = "1.0.0"
dependencies = ["serde", "sibling", "tempfile"]

[[package]]
name = "serde"
version = "1.0.210"
source = "{CRATES_IO}"

[[package]]
name = "sibling"
version = "0.1.0"

[[package]]
name = "tempfile"
version = "3.20.0"
source = "{CRATES_IO}"
"#
        ))
        .unwrap()
    }

    #[test]
    fn version_is_read_from_tool_output() {
        assert_eq!(
            version_from_output("rustc 1.98.1 (01f6ddf75 2026-08-05)\n").as_deref(),
            Some("1.98.1")
        );
        assert_eq!(
            version_from_output("cargo 1.98.1 (797e8a9bc 2026-08-05)").as_deref(),
            Some("1.98.1")
        );
        assert_eq!(
            version_from_output("rustc 1.99.0-nightly (abc 2026-09-01)").as_deref(),
            Some("1.99.0-nightly")
        );
        assert_eq!(version_from_output("error: no default toolchain"), None);
        assert_eq!(version_from_output(""), None);
    }

    #[test]
    fn cargo_projects_are_detected_by_their_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        assert!(!is_cargo_project(&root));
        assert!(!has_lockfile(&root));

        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(is_cargo_project(&root));
        assert!(!has_lockfile(&root));

        std::fs::write(dir.path().join("Cargo.lock"), "version = 4\n").unwrap();
        assert!(has_lockfile(&root));

        // A directory by that name is not a manifest.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join("Cargo.toml")).unwrap();
        assert!(!is_cargo_project(&other.path().to_string_lossy()));
        assert!(!is_cargo_project(&dir.path().join("missing").to_string_lossy()));
    }

    #[test]
    fn lockfile_is_read_from_the_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();

        let error = read_lockfile(&root).unwrap_err();
        assert!(error.contains("Cargo.lock"), "{error}");

        std::fs::write(
            dir.path().join("Cargo.lock"),
            "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        let lockfile = read_lockfile(&root).unwrap();
        assert_eq!(lockfile.packages.len(), 1);
        assert_eq!(lockfile.packages[0].name, "a");
    }

    #[test]
    fn listed_dependencies_carry_their_locked_version() {
        let krate = app(vec![
            dependency("serde", DependencyKind::Normal, DependencySource::CratesIo),
            dependency("tempfile", DependencyKind::Dev, DependencySource::CratesIo),
        ]);
        let list = direct_dependencies(&krate, Some(&lockfile()));

        let rows: Vec<(&str, DependencyKind, Option<&str>)> = list
            .listed
            .iter()
            .map(|row| (row.name.as_str(), row.kind, row.locked_version.as_deref()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("serde", DependencyKind::Normal, Some("1.0.210")),
                ("tempfile", DependencyKind::Dev, Some("3.20.0")),
            ]
        );
        assert_eq!(list.listed[0].requirement, "^1");
        assert_eq!(list.hidden, 0);
    }

    #[test]
    fn path_git_and_other_registry_dependencies_are_counted_not_listed() {
        let krate = app(vec![
            dependency("serde", DependencyKind::Normal, DependencySource::CratesIo),
            dependency(
                "sibling",
                DependencyKind::Normal,
                DependencySource::Path("/work/sibling".into()),
            ),
            // The same local crate again as a dev-dependency: still one.
            dependency(
                "sibling",
                DependencyKind::Dev,
                DependencySource::Path("/work/sibling".into()),
            ),
            dependency(
                "forked",
                DependencyKind::Normal,
                DependencySource::Git("https://example.test/forked".into()),
            ),
            dependency(
                "internal",
                DependencyKind::Normal,
                DependencySource::OtherRegistry("registry+https://example.test/index".into()),
            ),
        ]);
        let list = direct_dependencies(&krate, Some(&lockfile()));

        let names: Vec<&str> = list.listed.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["serde"]);
        assert_eq!(list.hidden, 3);
    }

    #[test]
    fn locked_version_is_unknown_without_a_lockfile_or_an_entry() {
        let krate = app(vec![
            dependency("serde", DependencyKind::Normal, DependencySource::CratesIo),
            // Added to Cargo.toml after the lockfile was last written.
            dependency("brand-new", DependencyKind::Normal, DependencySource::CratesIo),
        ]);

        let without_lockfile = direct_dependencies(&krate, None);
        assert_eq!(without_lockfile.listed.len(), 2);
        assert!(without_lockfile.listed.iter().all(|row| row.locked_version.is_none()));

        let with_lockfile = direct_dependencies(&krate, Some(&lockfile()));
        assert_eq!(with_lockfile.listed[0].locked_version.as_deref(), Some("1.0.210"));
        assert_eq!(with_lockfile.listed[1].name, "brand-new");
        assert_eq!(with_lockfile.listed[1].locked_version, None);
    }

    #[test]
    fn registry_names_are_distinct_and_in_list_order() {
        let mut windows_only = dependency("serde", DependencyKind::Dev, DependencySource::CratesIo);
        windows_only.target = Some("cfg(windows)".to_string());
        let krate = app(vec![
            dependency("serde", DependencyKind::Normal, DependencySource::CratesIo),
            windows_only,
            dependency("tempfile", DependencyKind::Dev, DependencySource::CratesIo),
            dependency("sibling", DependencyKind::Normal, DependencySource::Path("/w/s".into())),
        ]);
        let list = direct_dependencies(&krate, None);
        assert_eq!(list.listed.len(), 3);
        // Two rows for serde, one lookup; the hidden sibling needs none.
        assert_eq!(list.registry_names(), vec!["serde", "tempfile"]);
        assert!(DependencyList::default().registry_names().is_empty());
    }

    #[test]
    fn a_row_reports_its_status_against_the_registry() {
        let krate = app(vec![dependency(
            "serde",
            DependencyKind::Normal,
            DependencySource::CratesIo,
        )]);
        let list = direct_dependencies(&krate, Some(&lockfile()));
        let versions = parse_index(concat!(
            r#"{"vers":"1.0.210"}"#,
            "\n",
            r#"{"vers":"1.0.229","rust_version":"1.56"}"#,
            "\n",
            r#"{"vers":"2.0.0","rust_version":"1.99"}"#,
            "\n",
        ))
        .unwrap();

        // Declared `^1`, locked 1.0.210.
        let status = list.listed[0].status(&versions, Some("1.98.1"));
        let in_range = status.in_range.unwrap();
        assert_eq!(in_range.version, Version::new(1, 0, 229));
        assert_eq!(in_range.kind, UpdateKind::Patch);
        assert_eq!(in_range.rust, RustCompat::Compatible);

        let out_of_range = status.out_of_range.unwrap();
        assert_eq!(out_of_range.version, Version::new(2, 0, 0));
        assert_eq!(out_of_range.kind, UpdateKind::Major);
        assert_eq!(out_of_range.rust, RustCompat::TooNew { needs: "1.99".into() });
    }

    #[test]
    fn a_crate_with_no_dependencies() {
        let list = direct_dependencies(&app(Vec::new()), Some(&lockfile()));
        assert_eq!(list, DependencyList::default());
    }

    /// The acceptance check from the design note: the list must match
    /// `Cargo.toml` on a real crate. Runs `cargo metadata` against the
    /// workspace this crate lives in, so it is ignored by default (no other
    /// test in the fork's backends invokes its tool):
    ///
    ///     cargo test -p cargo_backend -- --ignored
    #[test]
    #[ignore = "runs cargo against the real workspace"]
    fn this_crate_is_listed_correctly_in_its_own_workspace() {
        let workspace = load_workspace(env!("CARGO_MANIFEST_DIR")).unwrap();
        assert!(is_cargo_project(&workspace.root));
        assert!(workspace.crates.len() > 1, "expected a multi-crate workspace");

        let krate = workspace.find("cargo_backend").unwrap();
        assert!(krate.manifest_path.ends_with("Cargo.toml"));

        let lockfile = read_lockfile(&workspace.root).unwrap();
        let list = direct_dependencies(krate, Some(&lockfile));
        let names: Vec<&str> = list.listed.iter().map(|row| row.name.as_str()).collect();

        // What crates/cargo_backend/Cargo.toml declares from crates.io.
        for expected in ["log", "serde", "serde_json", "tempfile", "toml", "windows-registry"] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
        // `gpui_util` is a workspace sibling: hidden, and counted.
        assert!(!names.contains(&"gpui_util"));
        assert_eq!(list.hidden, 1);

        // `serde.workspace = true` arrives with the workspace's requirement.
        let serde = list.listed.iter().find(|row| row.name == "serde").unwrap();
        assert!(serde.requirement.starts_with('^'), "{}", serde.requirement);
        assert_eq!(
            list.listed.iter().find(|row| row.name == "tempfile").unwrap().kind,
            DependencyKind::Dev
        );
        // Everything declared is in the lockfile.
        for row in &list.listed {
            assert!(row.locked_version.is_some(), "{} has no locked version", row.name);
        }

        assert!(!lockfile
            .reachable_crates_io_packages(&krate.name, &krate.version)
            .is_empty());
        assert!(query_cargo().is_ok());
        assert!(query_rustc().is_ok());
    }
}
