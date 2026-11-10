//! Scans a workspace root for runnable services (a React frontend, a Node
//! backend, a Python server, ...) and proposes a `command`/`args`/`cwd`/
//! `port` for each — the detection half of "Add Task Chain" (§ this
//! session's conversation on generating a multi-service `StartProcess` +
//! `WaitForPort` flow from an existing full-stack project). Deliberately
//! **guided, not fully automatic** (decided 2026-09-12): every guessed
//! `command`/`port` here is exactly that, a guess — real project layouts
//! vary too much (a custom port in `.env`, a monorepo script name that
//! isn't `"dev"`, ...) to generate a flow with no human review. The
//! caller — the Flows panel's `TaskChainWizard`
//! (`src/forge_shell/panels/flows_panel.rs`) — shows [`DetectedService`]s
//! for confirmation/editing before turning them into real actions.
//!
//! Pure data + file I/O, no `gpui` dependency, same convention as the rest
//! of this module.

use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    ReactFrontend,
    NodeBackend,
    /// A `package.json` exists but neither a frontend nor backend signal
    /// was found in it — still worth proposing (with a best-effort
    /// `command`), just not confidently one or the other.
    NodeApp,
    Django,
    Flask,
    FastApi,
    /// A Python project exists (`requirements.txt`/`pyproject.toml`/a
    /// `.py` entry point) but no recognized framework — same "still worth
    /// proposing" reasoning as `NodeApp`.
    PythonApp,
}

impl ServiceKind {
    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::ReactFrontend => "React Frontend",
            ServiceKind::NodeBackend => "Node Backend",
            ServiceKind::NodeApp => "Node App",
            ServiceKind::Django => "Django",
            ServiceKind::Flask => "Flask",
            ServiceKind::FastApi => "FastAPI",
            ServiceKind::PythonApp => "Python App",
        }
    }

    /// A common default dev-server port for this kind of project — a
    /// starting guess for the generated `WaitForPort`, not a detected
    /// fact (nothing here actually reads the project's real configured
    /// port; that would need parsing framework-specific config/`.env`
    /// files this doesn't attempt).
    pub fn default_port(self) -> u16 {
        match self {
            ServiceKind::ReactFrontend => 5173, // Vite's default; CRA's is 3000
            ServiceKind::NodeBackend | ServiceKind::NodeApp => 3000,
            ServiceKind::Django => 8000,
            ServiceKind::Flask => 5000,
            ServiceKind::FastApi => 8000,
            ServiceKind::PythonApp => 8000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectedService {
    pub name: String,
    pub kind: ServiceKind,
    /// Relative to the workspace root — becomes `StartProcess`'s `cwd`.
    pub relative_dir: String,
    pub command: String,
    pub args: Vec<String>,
    pub port: u16,
}

/// Scans `root` and its immediate subdirectories (not recursive beyond one
/// level — a monorepo's `frontend/`/`backend/`/`server/` convention, not a
/// deep tree walk) for `package.json`/Python project markers. A directory
/// that matches multiple signals only produces one `DetectedService` (the
/// most specific match wins — see `classify_node`/`classify_python`).
/// Missing/unreadable `root` is just an empty result, not an error —
/// nothing to propose either way.
pub fn scan(root: &Path) -> Vec<DetectedService> {
    let mut services = Vec::new();
    for dir in candidate_dirs(root) {
        if let Some(service) = detect_node(&dir, root) {
            services.push(service);
        } else if let Some(service) = detect_python(&dir, root) {
            services.push(service);
        }
    }
    services
}

/// `root` itself, plus every immediate subdirectory — covers both "the
/// whole workspace root is one Node project" and "workspace root holds
/// `frontend/`, `backend/`, `server/` as siblings."
fn candidate_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.') && n != "node_modules")
            {
                dirs.push(path);
            }
        }
    }
    dirs
}

fn relative_dir(dir: &Path, root: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        _ => ".".to_string(),
    }
}

fn detect_node(dir: &Path, root: &Path) -> Option<DetectedService> {
    let package_json_path = dir.join("package.json");
    let text = std::fs::read_to_string(&package_json_path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;

    let name = json
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "node-app".to_string())
        });

    let deps_have = |dep: &str| -> bool {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|section| json.get(section).and_then(|d| d.get(dep)).is_some())
    };
    let is_react =
        deps_have("react") || deps_have("vite") || deps_have("react-scripts") || deps_have("next");
    let is_backend = deps_have("express")
        || deps_have("fastify")
        || deps_have("koa")
        || deps_have("@nestjs/core");
    let kind = if is_react {
        ServiceKind::ReactFrontend
    } else if is_backend {
        ServiceKind::NodeBackend
    } else {
        ServiceKind::NodeApp
    };

    let scripts = json.get("scripts");
    let script_name = ["dev", "start", "serve"]
        .into_iter()
        .find(|name| scripts.and_then(|s| s.get(name)).is_some())
        .unwrap_or("start");

    Some(DetectedService {
        name,
        kind,
        relative_dir: relative_dir(dir, root),
        command: "npm".to_string(),
        args: vec!["run".to_string(), script_name.to_string()],
        port: kind.default_port(),
    })
}

fn detect_python(dir: &Path, root: &Path) -> Option<DetectedService> {
    let has_manage_py = dir.join("manage.py").is_file();
    let requirements = std::fs::read_to_string(dir.join("requirements.txt")).unwrap_or_default();
    let pyproject = std::fs::read_to_string(dir.join("pyproject.toml")).unwrap_or_default();
    let deps_text = format!("{requirements}\n{pyproject}").to_ascii_lowercase();

    let has_fastapi = deps_text.contains("fastapi") || deps_text.contains("uvicorn");
    let has_flask = deps_text.contains("flask");
    let has_django = has_manage_py || deps_text.contains("django");

    let main_py = ["main.py", "app.py", "server.py"]
        .iter()
        .find(|f| dir.join(f).is_file())
        .copied();

    if !has_django && !has_fastapi && !has_flask && main_py.is_none() {
        return None;
    }

    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "python-app".to_string());

    let (kind, command, args): (ServiceKind, &str, Vec<String>) = if has_django {
        (
            ServiceKind::Django,
            "python",
            vec!["manage.py".to_string(), "runserver".to_string()],
        )
    } else if has_fastapi {
        let entry = main_py.unwrap_or("main.py");
        let module = entry.trim_end_matches(".py");
        (
            ServiceKind::FastApi,
            "uvicorn",
            vec![format!("{module}:app"), "--reload".to_string()],
        )
    } else if has_flask {
        (ServiceKind::Flask, "flask", vec!["run".to_string()])
    } else {
        let entry = main_py.unwrap_or("main.py").to_string();
        (ServiceKind::PythonApp, "python", vec![entry])
    };

    // A project's packages live in its virtual environment, which a command
    // started from the IDE does not activate: a bare `python` would be the
    // system one and fail to import them. Use the environment's interpreter
    // when there is one, and run a framework's launcher through it as a module,
    // since `uvicorn` and `flask` are not on `PATH` either.
    let (command, args) = match venv_python(dir, root) {
        Some(python) => {
            let python = python.to_string_lossy().to_string();
            let module_args = |module: &str, args: Vec<String>| {
                let mut all = vec!["-m".to_string(), module.to_string()];
                all.extend(args);
                all
            };
            match kind {
                ServiceKind::FastApi => (python, module_args("uvicorn", args)),
                ServiceKind::Flask => (python, module_args("flask", args)),
                _ => (python, args),
            }
        }
        None => (command.to_string(), args),
    };

    Some(DetectedService {
        name,
        kind,
        relative_dir: relative_dir(dir, root),
        command,
        args,
        port: kind.default_port(),
    })
}

/// The interpreter of the project's virtual environment: `.venv` or `venv`,
/// in the project folder or in the workspace root (a monorepo usually has one
/// environment beside its `backend` folder). Its absolute path, so it works
/// whatever the working directory.
fn venv_python(dir: &Path, root: &Path) -> Option<PathBuf> {
    [dir, root]
        .into_iter()
        .flat_map(|base| [".venv", "venv"].map(|name| base.join(name)))
        .flat_map(|venv| {
            [
                venv.join("Scripts").join("python.exe"),
                venv.join("bin").join("python"),
            ]
        })
        .find(|python| python.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn detects_a_react_frontend_via_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let frontend = tmp.path().join("frontend");
        std::fs::create_dir(&frontend).unwrap();
        write(
            &frontend,
            "package.json",
            r#"{"name":"web","dependencies":{"react":"18.0.0","vite":"5.0.0"},"scripts":{"dev":"vite"}}"#,
        );

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "frontend")
            .unwrap();
        assert_eq!(found.kind, ServiceKind::ReactFrontend);
        assert_eq!(found.command, "npm");
        assert_eq!(found.args, vec!["run".to_string(), "dev".to_string()]);
        assert_eq!(found.name, "web");
    }

    #[test]
    fn detects_a_node_backend_via_express() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = tmp.path().join("backend");
        std::fs::create_dir(&backend).unwrap();
        write(
            &backend,
            "package.json",
            r#"{"name":"api","dependencies":{"express":"4.0.0"},"scripts":{"start":"node index.js"}}"#,
        );

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "backend")
            .unwrap();
        assert_eq!(found.kind, ServiceKind::NodeBackend);
        assert_eq!(found.args, vec!["run".to_string(), "start".to_string()]);
    }

    #[test]
    fn detects_django_via_manage_py() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir(&server).unwrap();
        write(&server, "manage.py", "# django");

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "server")
            .unwrap();
        assert_eq!(found.kind, ServiceKind::Django);
        assert_eq!(found.command, "python");
        assert_eq!(
            found.args,
            vec!["manage.py".to_string(), "runserver".to_string()]
        );
    }

    #[test]
    fn detects_fastapi_via_requirements_and_uses_the_real_entry_point() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir(&server).unwrap();
        write(&server, "requirements.txt", "fastapi\nuvicorn\n");
        write(&server, "app.py", "# fastapi app");

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "server")
            .unwrap();
        assert_eq!(found.kind, ServiceKind::FastApi);
        assert_eq!(found.command, "uvicorn");
        assert_eq!(
            found.args,
            vec!["app:app".to_string(), "--reload".to_string()]
        );
    }

    #[test]
    fn detects_flask_via_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        let server = tmp.path().join("server");
        std::fs::create_dir(&server).unwrap();
        write(&server, "requirements.txt", "flask\n");

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "server")
            .unwrap();
        assert_eq!(found.kind, ServiceKind::Flask);
    }

    #[test]
    fn a_full_stack_layout_detects_all_three_services_independently() {
        // Exactly the shape from the original request: React frontend +
        // Node backend + Python server as sibling directories.
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["frontend", "backend", "pyserver"] {
            std::fs::create_dir(tmp.path().join(dir)).unwrap();
        }
        write(
            &tmp.path().join("frontend"),
            "package.json",
            r#"{"name":"frontend","dependencies":{"react":"18.0.0"},"scripts":{"dev":"vite"}}"#,
        );
        write(
            &tmp.path().join("backend"),
            "package.json",
            r#"{"name":"backend","dependencies":{"express":"4.0.0"},"scripts":{"start":"node index.js"}}"#,
        );
        write(
            &tmp.path().join("pyserver"),
            "requirements.txt",
            "fastapi\nuvicorn\n",
        );
        write(&tmp.path().join("pyserver"), "main.py", "# app");

        let services = scan(tmp.path());
        assert_eq!(services.len(), 3);
        assert!(
            services
                .iter()
                .any(|s| s.kind == ServiceKind::ReactFrontend)
        );
        assert!(services.iter().any(|s| s.kind == ServiceKind::NodeBackend));
        assert!(services.iter().any(|s| s.kind == ServiceKind::FastApi));
    }

    #[test]
    fn an_empty_directory_detects_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan(tmp.path()).is_empty());
    }

    #[test]
    fn a_missing_directory_is_empty_not_an_error() {
        let missing = Path::new("this/does/not/exist/at/all");
        assert!(scan(missing).is_empty());
    }

    #[test]
    fn node_modules_is_never_scanned_as_a_candidate_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let node_modules = tmp.path().join("node_modules");
        std::fs::create_dir(&node_modules).unwrap();
        write(
            &node_modules,
            "package.json",
            r#"{"name":"some-dep","dependencies":{"react":"1.0.0"}}"#,
        );

        assert!(scan(tmp.path()).is_empty());
    }

    fn fake_venv_python(base: &Path, venv: &str) -> PathBuf {
        let scripts = base.join(venv).join("Scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let python = scripts.join("python.exe");
        std::fs::write(&python, "").unwrap();
        python
    }

    #[test]
    fn django_uses_the_virtual_environments_python_from_the_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = tmp.path().join("backend");
        std::fs::create_dir(&backend).unwrap();
        write(&backend, "manage.py", "# django");
        let python = fake_venv_python(tmp.path(), ".venv");

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "backend")
            .unwrap();
        assert_eq!(found.kind, ServiceKind::Django);
        assert_eq!(found.command, python.to_string_lossy());
        assert_eq!(
            found.args,
            vec!["manage.py".to_string(), "runserver".to_string()]
        );
    }

    #[test]
    fn a_project_folders_own_environment_wins_over_the_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = tmp.path().join("backend");
        std::fs::create_dir(&backend).unwrap();
        write(&backend, "manage.py", "# django");
        fake_venv_python(tmp.path(), ".venv");
        let own = fake_venv_python(&backend, "venv");

        let services = scan(tmp.path());
        let found = services
            .iter()
            .find(|s| s.relative_dir == "backend")
            .unwrap();
        assert_eq!(found.command, own.to_string_lossy());
    }

    #[test]
    fn fastapi_and_flask_run_as_modules_of_the_environments_python() {
        let tmp = tempfile::tempdir().unwrap();
        let api = tmp.path().join("api");
        let web = tmp.path().join("web");
        std::fs::create_dir(&api).unwrap();
        std::fs::create_dir(&web).unwrap();
        write(&api, "requirements.txt", "fastapi\nuvicorn\n");
        write(&web, "requirements.txt", "flask\n");
        let python = fake_venv_python(tmp.path(), ".venv");
        let python = python.to_string_lossy().to_string();

        let services = scan(tmp.path());
        let api = services.iter().find(|s| s.relative_dir == "api").unwrap();
        assert_eq!(api.command, python);
        assert_eq!(
            api.args,
            vec![
                "-m".to_string(),
                "uvicorn".to_string(),
                "main:app".to_string(),
                "--reload".to_string()
            ]
        );
        let web = services.iter().find(|s| s.relative_dir == "web").unwrap();
        assert_eq!(web.command, python);
        assert_eq!(
            web.args,
            vec!["-m".to_string(), "flask".to_string(), "run".to_string()]
        );
    }
}
