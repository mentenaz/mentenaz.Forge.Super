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
            .map(|path| normalize(&resolve(path)))
            .collect(),
        restricted: restricted
            .iter()
            .map(|path| normalize(&resolve(path)))
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
    let path = normalize(&resolve(path.as_ref()));
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

/// What the gate decided about one request, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Allow(&'static str),
    BlockRestricted(&'static str),
    BlockUnchecked(&'static str),
}

impl Verdict {
    fn is_allowed(self) -> bool {
        matches!(self, Verdict::Allow(_))
    }

    fn reason(self) -> &'static str {
        match self {
            Verdict::Allow(reason)
            | Verdict::BlockRestricted(reason)
            | Verdict::BlockUnchecked(reason) => reason,
        }
    }

    fn result(self) -> Result<(), String> {
        match self {
            Verdict::Allow(_) => Ok(()),
            Verdict::BlockRestricted(_) => Err(RESTRICTED_MESSAGE.to_string()),
            Verdict::BlockUnchecked(_) => Err(UNCHECKED_MESSAGE.to_string()),
        }
    }
}

/// The verdict for starting something in the project folder `path`: only a folder
/// the editor has marked trusted passes. An empty folder means the process's own.
fn decide_folder(path: &Path) -> Verdict {
    let own;
    let path = if path.as_os_str().is_empty() {
        own = std::env::current_dir().unwrap_or_default();
        own.as_path()
    } else {
        path
    };
    match classify(path) {
        Trust::Trusted => Verdict::Allow("the folder is trusted"),
        Trust::Restricted => Verdict::BlockRestricted("the folder is in Restricted Mode"),
        Trust::Unclassified => {
            Verdict::BlockUnchecked("the folder has not been classified as trusted yet")
        }
    }
}

/// The verdict for starting `program`, judged by the file it resolves to.
fn decide_program(program: &Path) -> Verdict {
    let resolved = resolve_program(program);
    match resolved.as_deref().map_or(Trust::Unclassified, classify) {
        Trust::Trusted => Verdict::Allow("the program is inside a trusted folder"),
        Trust::Restricted => {
            Verdict::BlockRestricted("the program is inside a folder in Restricted Mode")
        }
        Trust::Unclassified if is_settled() => {
            Verdict::Allow("the program is outside every project")
        }
        Trust::Unclassified => Verdict::BlockUnchecked("projects are still being classified"),
    }
}

/// Whether a program may be started in `path`, a project folder: only when the
/// editor has marked it trusted. A folder it has not classified yet, and one in
/// Restricted Mode, are both refused, with the message to show.
pub fn check(path: impl AsRef<Path>) -> Result<(), String> {
    if !lock().enforcing {
        return Ok(());
    }
    decide_folder(path.as_ref()).result()
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
    decide_program(Path::new(program.as_ref())).result()
}

/// What every backend calls just before it starts something, and the one place a
/// decision is written to the log, whether it allows the request or blocks it.
///
/// * `who` names the code that asked, for the log (the backend function; the
///   file and line of the call are added from the call site).
/// * `program` and `args` are what is about to be started.
/// * `folder` is the project folder it runs in. With a folder, the folder decides
///   (it must be trusted); without one the program is judged by where it lives.
///
/// Returns the error to show when the request is blocked. Before the editor has
/// switched the gate on, nothing is checked and nothing is logged.
#[track_caller]
pub fn check_run(
    who: &str,
    program: impl AsRef<OsStr>,
    args: &[&str],
    folder: Option<&Path>,
) -> Result<(), String> {
    let caller = std::panic::Location::caller();
    if !lock().enforcing {
        return Ok(());
    }
    let program = program.as_ref().to_string_lossy();
    let verdict = match folder {
        Some(folder) => decide_folder(folder),
        None => decide_program(Path::new(&*program)),
    };
    let line = describe(
        verdict,
        who,
        &format!("{}:{}", caller.file(), caller.line()),
        &program,
        args,
        folder,
    );
    if verdict.is_allowed() {
        log::info!("{line}");
    } else {
        log::warn!("{line}");
    }
    verdict.result()
}

/// The longest command text a log line carries, so a long script does not flood
/// the log.
const LOG_COMMAND_LIMIT: usize = 300;

/// One log line: the decision, the program and its arguments, the folder, who
/// asked and why.
fn describe(
    verdict: Verdict,
    who: &str,
    call_site: &str,
    program: &str,
    args: &[&str],
    folder: Option<&Path>,
) -> String {
    let mut command = program.to_string();
    for arg in args {
        command.push(' ');
        if arg.contains(' ') {
            command.push('"');
            command.push_str(arg);
            command.push('"');
        } else {
            command.push_str(arg);
        }
    }
    if command.chars().count() > LOG_COMMAND_LIMIT {
        command = command.chars().take(LOG_COMMAND_LIMIT).collect::<String>() + "...";
    }
    format!(
        "forge_trust_gate: {} {command} | folder: {} | asked by: {who} ({call_site}) | because {}",
        if verdict.is_allowed() {
            "ALLOWED"
        } else {
            "BLOCKED"
        },
        folder.map_or_else(
            || "(none)".to_string(),
            |folder| folder.display().to_string()
        ),
        verdict.reason(),
    )
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

/// Where `path` really is. A folder is judged by its real location, not by how a
/// path to it is spelled: `repo/../elsewhere` and a symlink inside a trusted folder
/// that points out of it are not inside it. Comparing the spelling would let both
/// pass (`std::path::absolute` keeps `..` on Linux and macOS and never follows
/// symlinks).
///
/// A path that exists is resolved by the file system. One that does not exist yet
/// (a folder about to be created) keeps its missing tail, after the part that
/// exists is resolved. The trusted and restricted roots are resolved the same way,
/// so both sides of a comparison are real paths.
fn resolve(path: &Path) -> PathBuf {
    let absolute = absolute(path);
    if let Ok(real) = std::fs::canonicalize(&absolute) {
        return real;
    }
    let collapsed = collapse(&absolute);
    let mut missing = Vec::new();
    let mut existing = collapsed.as_path();
    loop {
        if let Ok(mut real) = std::fs::canonicalize(existing) {
            real.extend(missing.iter().rev());
            return real;
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name.to_owned());
                existing = parent;
            }
            _ => return collapsed,
        }
    }
}

/// `path` without its `.` and `..` components, by the spelling alone. Only for the
/// part of a path that does not exist, where there is no symlink to follow.
fn collapse(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never above the root.
                if !matches!(
                    out.components().next_back(),
                    None | Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
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
    fn check_run_decides_like_check_and_check_program() {
        let _turn = fresh();
        assert_eq!(
            check_run(
                "test",
                "cargo",
                &["metadata"],
                Some(Path::new("/work/repo"))
            ),
            Ok(()),
            "nothing is checked before the gate is switched on"
        );

        enforce();
        classified(&["/work/ok"], &["/work/repo"]);
        assert_eq!(
            check_run("test", "cargo", &["metadata"], Some(Path::new("/work/ok"))),
            Ok(())
        );
        assert_eq!(
            check_run(
                "test",
                "cargo",
                &["metadata"],
                Some(Path::new("/work/repo"))
            ),
            Err(RESTRICTED_MESSAGE.to_string())
        );
        assert_eq!(
            check_run("test", "cargo", &[], Some(Path::new("/work/new"))),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        // Without a folder the program is judged by where it lives.
        assert_eq!(
            check_run("test", "/work/repo/.venv/bin/python", &["-m", "pip"], None),
            Err(RESTRICTED_MESSAGE.to_string())
        );
        assert_eq!(check_run("test", "/usr/bin/python3", &[], None), Ok(()));
    }

    #[test]
    fn a_log_line_names_the_decision_program_arguments_folder_and_who_asked() {
        let blocked = describe(
            Verdict::BlockRestricted("the folder is in Restricted Mode"),
            "cargo_backend::run_cargo_output",
            "crates/cargo_backend/src/cargo_backend.rs:112",
            "cargo",
            &["metadata", "--no-deps", "--format-version", "1"],
            Some(Path::new("E:/canary-test/project")),
        );
        assert_eq!(
            blocked,
            "forge_trust_gate: BLOCKED cargo metadata --no-deps --format-version 1 \
             | folder: E:/canary-test/project \
             | asked by: cargo_backend::run_cargo_output \
             (crates/cargo_backend/src/cargo_backend.rs:112) \
             | because the folder is in Restricted Mode"
        );

        let allowed = describe(
            Verdict::Allow("the program is outside every project"),
            "python_backend::run_captured",
            "crates/python_backend/src/python_backend.rs:105",
            "python",
            &["-m", "pip", "list", "--format", "my json"],
            None,
        );
        assert!(allowed.contains("ALLOWED python -m pip list --format \"my json\""));
        assert!(allowed.contains("folder: (none)"));
        assert!(allowed.contains("asked by: python_backend::run_captured"));
    }

    #[test]
    fn a_long_command_is_cut_in_the_log() {
        let script = "x".repeat(1000);
        let line = describe(Verdict::Allow("ok"), "who", "here:1", &script, &[], None);
        assert!(line.contains(&format!("{}...", "x".repeat(LOG_COMMAND_LIMIT))));
        assert!(!line.contains(&"x".repeat(LOG_COMMAND_LIMIT + 1)));
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

    /// A folder of its own under the temp folder, made fresh for one test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("forge_trust_gate_{}_{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A link at `link` to the folder `target`: a symlink, or on Windows a junction
    /// when symlinks are not allowed. False when neither could be made.
    fn link_folder(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
                || std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(link)
                    .arg(target)
                    .output()
                    .is_ok_and(|out| out.status.success())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            false
        }
    }

    #[test]
    fn dot_dot_cannot_leave_a_trusted_folder() {
        let _turn = fresh();
        enforce();
        classified(&["/work/repo"], &[]);
        assert_eq!(
            check("/work/repo/../elsewhere"),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        assert_eq!(
            check("/work/repo/sub/../../elsewhere"),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        // Staying inside still works, and `.` changes nothing.
        assert_eq!(check("/work/repo/sub/../other"), Ok(()));
        assert_eq!(check("/work/repo/./sub"), Ok(()));
        // Above the root is the root, not a way round it.
        assert_eq!(check("/../work/repo/x"), Ok(()));
    }

    #[test]
    fn a_link_out_of_a_trusted_folder_does_not_carry_trust() {
        let _turn = fresh();
        let scratch = Scratch::new("link_out");
        let trusted = scratch.0.join("trusted");
        let outside = scratch.0.join("outside");
        std::fs::create_dir_all(trusted.join("pkg")).unwrap();
        std::fs::create_dir_all(outside.join("evil")).unwrap();
        if !link_folder(&outside.join("evil"), &trusted.join("link")) {
            eprintln!("no folder link could be made here; skipping");
            return;
        }
        enforce();
        begin_project(1);
        set_project_roots(1, vec![trusted.clone()], Vec::new());

        assert_eq!(check(trusted.join("pkg")), Ok(()));
        // The link looks inside the trusted folder; where it leads is not.
        assert_eq!(
            check(trusted.join("link")),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        assert_eq!(
            check(trusted.join("link").join("not-made-yet")),
            Err(UNCHECKED_MESSAGE.to_string())
        );
        assert_eq!(classify(outside.join("evil")), Trust::Unclassified);
    }

    #[test]
    fn a_link_to_a_trusted_folder_is_that_folder() {
        let _turn = fresh();
        let scratch = Scratch::new("link_in");
        let repo = scratch.0.join("repo");
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        let alias = scratch.0.join("alias");
        if !link_folder(&repo, &alias) {
            eprintln!("no folder link could be made here; skipping");
            return;
        }
        enforce();
        begin_project(1);
        // The editor gives the real folder; a path through the alias is the same place.
        set_project_roots(1, vec![repo.clone()], Vec::new());
        assert_eq!(check(alias.join("sub")), Ok(()));
        // And the other way round: trusted by the alias, reached by the real path.
        set_project_roots(1, vec![alias.clone()], Vec::new());
        assert_eq!(check(repo.join("sub")), Ok(()));
    }

    #[test]
    fn a_restricted_folder_is_still_restricted_through_a_link() {
        let _turn = fresh();
        let scratch = Scratch::new("link_restricted");
        let work = scratch.0.join("work");
        let secret = work.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        let alias = scratch.0.join("alias");
        if !link_folder(&secret, &alias) {
            eprintln!("no folder link could be made here; skipping");
            return;
        }
        enforce();
        begin_project(1);
        set_project_roots(1, vec![work.clone()], vec![secret.clone()]);
        assert_eq!(check(&alias), Err(RESTRICTED_MESSAGE.to_string()));
    }
}
