//! Which project folders the editor has said are trusted, which the code that
//! starts programs for Forge's panels asks about before it starts one in (or
//! from) a project.
//!
//! The editor owns the trust decision (`project::trusted_worktrees`, the same one
//! that keeps tasks and language servers from running in Restricted Mode). The
//! backends that run `cargo`, `dotnet`, `npm` or `python` are plain crates that
//! work on background threads and cannot reach it. So the editor side
//! (`forge_trust`) publishes, per project, the folders it has marked trusted and
//! the ones that are restricted, and a backend calls [`check`] or
//! [`check_program`] before it spawns.
//!
//! The gate fails closed. Once the editor has switched it on ([`enforce`]), a
//! folder is allowed only after the editor has explicitly marked it trusted; one
//! it has not classified yet is blocked, so a startup race can only delay a scan,
//! never allow one. Before [`enforce`] is called (tests and examples that use a
//! backend on its own) everything is allowed.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// What a panel shows when a backend refused to run because the project is in
/// Restricted Mode.
pub const RESTRICTED_MESSAGE: &str = "This project is not trusted yet, so Forge Super has not run anything from it. Choose \"Trust this project to scan it\" to allow it.";

/// What a panel shows when a backend refused to run because the editor has not
/// decided yet. It passes on its own: the panel scans again once it has.
pub const UNCHECKED_MESSAGE: &str = "Forge Super is still checking whether this project is trusted, so it has not run anything from it yet. It will scan again as soon as it knows.";

/// How a path stands with the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Inside a folder the editor has marked trusted.
    Trusted,
    /// Inside a folder that is in Restricted Mode.
    Restricted,
    /// Inside neither: the editor has not said anything about it.
    Unclassified,
}

/// The folders one project has classified, as normalised text.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Roots {
    trusted: Vec<String>,
    restricted: Vec<String>,
}

#[derive(Debug)]
struct State {
    enforcing: bool,
    /// Each project the editor follows, by id. `None` until its first
    /// classification, which is what keeps [`check_program`] waiting at startup.
    projects: BTreeMap<u64, Option<Roots>>,
}

static STATE: Mutex<State> = Mutex::new(State {
    enforcing: false,
    projects: BTreeMap::new(),
});

fn lock() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Switches the gate on. From here a folder must be marked trusted before
/// anything is started in it. The editor calls this before it opens any project.
pub fn enforce() {
    lock().enforcing = true;
}

/// Tells the gate a project exists that has not been classified yet. Until
/// [`set_project_roots`] is called for it, [`check_program`] holds back any
/// program it cannot place, since it could live inside that project.
pub fn begin_project(owner: u64) {
    let mut state = lock();
    state.enforcing = true;
    state.projects.entry(owner).or_insert(None);
}

/// Publishes how one project's folders are classified. Returns whether this
/// changed what the project had published before (publishing for the first time
/// always counts).
pub fn set_project_roots(owner: u64, trusted: Vec<PathBuf>, restricted: Vec<PathBuf>) -> bool {
    let roots = Roots {
        trusted: trusted
            .iter()
            .map(|path| normalize(&absolute(path)))
            .collect(),
        restricted: restricted
            .iter()
            .map(|path| normalize(&absolute(path)))
            .collect(),
    };
    let previous = lock().projects.insert(owner, Some(roots.clone()));
    previous != Some(Some(roots))
}

/// Forgets a project, once it is gone.
pub fn clear_owner(owner: u64) {
    lock().projects.remove(&owner);
}

/// Whether every project the editor follows has been classified at least once.
pub fn is_settled() -> bool {
    let state = lock();
    state.enforcing && state.projects.values().all(Option::is_some)
}

/// How `path` stands. A restricted folder wins over a trusted one that contains
/// it.
pub fn classify(path: impl AsRef<Path>) -> Trust {
    let path = normalize(&absolute(path.as_ref()));
    let state = lock();
    let roots = || state.projects.values().flatten();
    if roots().any(|r| r.restricted.iter().any(|root| is_inside(&path, root))) {
        Trust::Restricted
    } else if roots().any(|r| r.trusted.iter().any(|root| is_inside(&path, root))) {
        Trust::Trusted
    } else {
        Trust::Unclassified
    }
}

/// Whether a program may be started in `path`, a project folder: only when the
/// editor has marked it trusted. A folder it has not classified yet, and one in
/// Restricted Mode, are both refused, with the message to show.
pub fn check(path: impl AsRef<Path>) -> Result<(), String> {
    if !lock().enforcing {
        return Ok(());
    }
    // An empty folder means the process's own.
    let path = path.as_ref();
    let own;
    let path = if path.as_os_str().is_empty() {
        own = std::env::current_dir().unwrap_or_default();
        own.as_path()
    } else {
        path
    };
    match classify(path) {
        Trust::Trusted => Ok(()),
        Trust::Restricted => Err(RESTRICTED_MESSAGE.to_string()),
        Trust::Unclassified => Err(UNCHECKED_MESSAGE.to_string()),
    }
}

/// Whether `program` may be started. It is resolved first (a bare name is looked
/// up on `PATH`, a relative path is made absolute), and the resolved file is what
/// is judged, so a project's own virtual environment is caught whether it is named
/// by its path or reached through `PATH`.
///
/// A program inside a trusted folder is allowed, one inside a restricted folder
/// is refused. One in neither is a program the editor knows nothing about: a
/// system tool such as `python` or `dotnet`. That is allowed only once every
/// project has been classified, because until then it could be inside one that
/// has not been.
pub fn check_program(program: impl AsRef<OsStr>) -> Result<(), String> {
    if !lock().enforcing {
        return Ok(());
    }
    let resolved = resolve_program(Path::new(program.as_ref()));
    match resolved.as_deref().map_or(Trust::Unclassified, classify) {
        Trust::Trusted => Ok(()),
        Trust::Restricted => Err(RESTRICTED_MESSAGE.to_string()),
        Trust::Unclassified if is_settled() => Ok(()),
        Trust::Unclassified => Err(UNCHECKED_MESSAGE.to_string()),
    }
}

/// The file `program` would start: itself when it names a path, otherwise the
/// first match on `PATH` (and, on Windows, in the current folder first, with the
/// usual extensions). `None` when it cannot be found.
pub fn resolve_program(program: &Path) -> Option<PathBuf> {
    let names_a_path = program.is_absolute() || program.components().count() > 1;
    if names_a_path {
        return Some(absolute(program));
    }

    let mut directories = Vec::new();
    if cfg!(windows) {
        directories.extend(std::env::current_dir().ok());
    }
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };

    let name = program.as_os_str().to_string_lossy().into_owned();
    for directory in directories {
        let plain = directory.join(&name);
        if plain.is_file() {
            return Some(plain);
        }
        for extension in &extensions {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Forward slashes, no `\\?\` prefix, no trailing slash, and lower case on
/// Windows, where the file system does not tell `Proj` from `proj`.
fn normalize(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = text.strip_prefix("//?/") {
        text = stripped.to_string();
    }
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    if cfg!(windows) {
        text = text.to_lowercase();
    }
    text
}

fn is_inside(path: &str, root: &str) -> bool {
    if root == "/" {
        return true;
    }
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The state is process-wide, so tests that touch it take turns.
    static TURN: Mutex<()> = Mutex::new(());

    fn fresh() -> MutexGuard<'static, ()> {
        let turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *lock() = State {
            enforcing: false,
            projects: BTreeMap::new(),
        };
        turn
    }

    /// One project, classified, with the given trusted and restricted folders.
    fn classified(trusted: &[&str], restricted: &[&str]) {
        begin_project(1);
        set_project_roots(
            1,
            trusted.iter().map(PathBuf::from).collect(),
            restricted.iter().map(PathBuf::from).collect(),
        );
    }

    #[test]
    fn everything_is_allowed_until_the_editor_switches_the_gate_on() {
        let _turn = fresh();
        assert_eq!(check("/anywhere"), Ok(()));
        assert_eq!(check_program("/anywhere/python"), Ok(()));
    }

    #[test]
    fn an_unknown_path_is_blocked() {
        let _turn = fresh();
        enforce();
        assert_eq!(check("/work/repo"), Err(UNCHECKED_MESSAGE.to_string()));

        // Still blocked while the project exists but has not been classified...
        begin_project(1);
        assert_eq!(check("/work/repo"), Err(UNCHECKED_MESSAGE.to_string()));
        // ...and when it has been classified without this folder in it.
        classified(&["/work/other"], &[]);
        assert_eq!(check("/work/repo"), Err(UNCHECKED_MESSAGE.to_string()));
        assert_eq!(classify("/work/repo"), Trust::Unclassified);
    }

    #[test]
    fn a_trusted_path_is_allowed() {
        let _turn = fresh();
        enforce();
        classified(&["/work/repo"], &[]);
        assert_eq!(check("/work/repo"), Ok(()));
        assert_eq!(check("/work/repo/"), Ok(()));
        assert_eq!(classify("/work/repo"), Trust::Trusted);
    }

    #[test]
    fn a_path_inside_a_trusted_folder_is_allowed() {
        let _turn = fresh();
        enforce();
        classified(&["/work/repo"], &[]);
        assert_eq!(check("/work/repo/crates/a"), Ok(()));
        assert_eq!(check("/work/repo/.venv/bin"), Ok(()));
    }

    #[test]
    fn a_sibling_with_a_similar_prefix_is_not_allowed() {
        let _turn = fresh();
        enforce();
        classified(&["/work/repo"], &[]);
        assert_eq!(check("/work/repo2"), Err(UNCHECKED_MESSAGE.to_string()));
        assert_eq!(
            check("/work/repo-old/x"),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        assert_eq!(check("/work"), Err(UNCHECKED_MESSAGE.to_string()));
    }

    #[test]
    fn a_restricted_folder_is_blocked_and_wins_over_a_trusted_parent() {
        let _turn = fresh();
        enforce();
        classified(&["/work"], &["/work/repo"]);
        assert_eq!(check("/work/repo/x"), Err(RESTRICTED_MESSAGE.to_string()));
        assert_eq!(check("/work/other"), Ok(()));
    }

    #[test]
    fn classifying_a_project_for_the_first_time_counts_as_a_change() {
        let _turn = fresh();
        enforce();
        begin_project(1);
        assert!(set_project_roots(1, Vec::new(), Vec::new()), "first time");
        assert!(!set_project_roots(1, Vec::new(), Vec::new()), "same again");
        assert!(set_project_roots(
            1,
            vec![PathBuf::from("/work/repo")],
            Vec::new()
        ));
    }

    #[test]
    fn a_program_the_gate_cannot_place_waits_until_every_project_is_classified() {
        let _turn = fresh();
        enforce();
        begin_project(1);
        assert!(!is_settled());
        // A system tool, or the project's own virtual environment: it cannot tell
        // which yet, so it waits.
        assert_eq!(
            check_program("/usr/bin/python3"),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        assert_eq!(
            check_program("/work/repo/.venv/bin/python"),
            Err(UNCHECKED_MESSAGE.to_string())
        );

        set_project_roots(1, vec![PathBuf::from("/work/repo")], Vec::new());
        assert!(is_settled());
        assert_eq!(check_program("/usr/bin/python3"), Ok(()));
        assert_eq!(check_program("/work/repo/.venv/bin/python"), Ok(()));
    }

    #[test]
    fn a_program_inside_a_restricted_folder_is_blocked_even_when_settled() {
        let _turn = fresh();
        enforce();
        classified(&["/work/ok"], &["/work/repo"]);
        assert!(is_settled());
        assert_eq!(
            check_program("/work/repo/.venv/Scripts/python.exe"),
            Err(RESTRICTED_MESSAGE.to_string())
        );
        assert_eq!(check_program("/work/ok/.venv/bin/python"), Ok(()));
        assert_eq!(check_program("/usr/bin/python3"), Ok(()));
    }

    #[test]
    fn a_bare_program_name_is_judged_by_the_file_it_resolves_to() {
        let _turn = fresh();
        let directory =
            std::env::temp_dir().join(format!("forge_trust_gate_{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let name = "forge_trust_gate_canary_tool";
        let file = directory.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        });
        std::fs::write(&file, "").unwrap();

        let original = std::env::var_os("PATH");
        let mut entries = vec![directory.clone()];
        entries.extend(original.iter().flat_map(std::env::split_paths));
        // SAFETY: this test is the only one that changes the environment, and the
        // tests that read it hold the same turn.
        unsafe { std::env::set_var("PATH", std::env::join_paths(entries).unwrap()) };

        assert_eq!(
            resolve_program(Path::new(name)).map(|found| normalize(&found)),
            Some(normalize(&file))
        );

        enforce();
        classified(&[], &[directory.to_str().unwrap()]);
        assert_eq!(
            check_program(name),
            Err(RESTRICTED_MESSAGE.to_string()),
            "a bare name that lands in a restricted folder is refused"
        );
        classified(&[directory.to_str().unwrap()], &[]);
        assert_eq!(check_program(name), Ok(()));

        match original {
            Some(original) => unsafe { std::env::set_var("PATH", original) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn an_empty_folder_means_the_processes_own() {
        let _turn = fresh();
        enforce();
        let own = std::env::current_dir().unwrap();
        classified(&[own.to_str().unwrap()], &[]);
        assert_eq!(check(""), Ok(()));
    }

    #[test]
    fn windows_style_paths_compare_the_same_way() {
        assert!(is_inside("c:/proj/sub", "c:/proj"));
        assert!(!is_inside("c:/proj2", "c:/proj"));
        assert_eq!(normalize(Path::new("C:\\Proj\\")).to_lowercase(), "c:/proj");
        assert_eq!(
            normalize(Path::new("\\\\?\\C:\\Proj")).to_lowercase(),
            "c:/proj"
        );
    }
}
