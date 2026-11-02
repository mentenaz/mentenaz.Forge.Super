//! Prints which of a crate's dependencies are behind the registry — step 2
//! of the manager, from a terminal.
//!
//!     cargo run -p cargo_backend --example outdated -- <directory> [crate-name]
//!
//! This is the one place in the crate that touches the network. The library
//! leaves requests to its host; the example stands in for one by shelling
//! out to `curl`, one request per distinct dependency.

use cargo_backend::{
    RustCompat, UpdateTarget, direct_dependencies, has_lockfile, index_url, load_workspace,
    parse_index, read_lockfile,
};

const USER_AGENT: &str = "zed-fork cargo_backend example (https://github.com/mentenaz/mentenaz.Forge.Super)";

#[allow(clippy::disallowed_methods)]
fn fetch(url: &str) -> Result<String, String> {
    let output = std::process::Command::new("curl")
        .args([
            "--silent",
            "--fail",
            "--max-time",
            "20",
            "--user-agent",
            USER_AGENT,
            url,
        ])
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!("request failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn describe(target: &UpdateTarget) -> String {
    let rust = match &target.rust {
        RustCompat::Compatible => String::new(),
        RustCompat::TooNew { needs } => format!(" [needs Rust {needs}]"),
        RustCompat::NotDeclared => " [Rust version not declared]".to_string(),
    };
    format!("{} ({}){rust}", target.version, target.kind.label())
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let directory = args.next().unwrap_or_else(|| ".".to_string());
    let crate_name = args.next();

    let toolchain = cargo_backend::query_rustc().ok();
    println!("rustc {}", toolchain.as_deref().unwrap_or("unknown"));

    let workspace = load_workspace(&directory)?;
    let krate = match &crate_name {
        Some(name) => workspace
            .find(name)
            .ok_or_else(|| format!("no crate named {name:?} in this workspace"))?,
        None => workspace
            .default_crate()
            .ok_or_else(|| "the workspace has no crates".to_string())?,
    };
    println!("crate: {} {}", krate.name, krate.version);

    let lockfile = if has_lockfile(&workspace.root) {
        Some(read_lockfile(&workspace.root)?)
    } else {
        None
    };
    let list = direct_dependencies(krate, lockfile.as_ref());

    let mut index = std::collections::HashMap::new();
    for name in list.registry_names() {
        match fetch(&index_url(name)).and_then(|body| parse_index(&body)) {
            Ok(versions) => {
                index.insert(name.to_string(), versions);
            }
            Err(error) => println!("  {name}: lookup failed: {error}"),
        }
    }

    let mut outdated = 0;
    for row in &list.listed {
        let Some(versions) = index.get(&row.name) else {
            continue;
        };
        let status = row.status(versions, toolchain.as_deref());
        if !status.is_outdated() && !status.locked_yanked {
            continue;
        }
        outdated += 1;
        println!(
            "  {:<24} {:<10} locked {}",
            row.name,
            row.requirement,
            row.locked_version.as_deref().unwrap_or("unknown")
        );
        if status.locked_yanked {
            println!("      the locked version has been yanked");
        }
        if let Some(target) = &status.in_range {
            println!("      cargo update {:<28} -> {}", row.name, describe(target));
        }
        if let Some(target) = &status.out_of_range {
            println!(
                "      cargo add {:<31} -> {}",
                format!("{}@{}", row.name, target.version),
                describe(target)
            );
        }
    }
    println!(
        "{outdated} of {} listed dependencies are behind ({} lookups)",
        list.listed.len(),
        index.len()
    );
    Ok(())
}
