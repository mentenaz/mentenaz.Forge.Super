//! Prints the security advisories affecting a crate's dependencies — step 3
//! of the manager, from a terminal.
//!
//!     cargo run -p cargo_backend --example advisories -- <directory> [crate-name]
//!
//! Like `outdated`, this stands in for a host by shelling out to `curl`:
//! one batch request per 1,000 packages, then one request per advisory,
//! eight at a time.

use std::io::Write as _;
use std::process::{Command, Stdio};

use cargo_backend::{
    AdvisoryRecord, FindingCounts, FindingKind, OSV_BATCH_URL, advisory_ids, advisory_url,
    load_workspace, merge_findings, osv_batches, parse_advisory, read_lockfile,
};

const USER_AGENT: &str = "zed-fork cargo_backend example (https://github.com/mentenaz/mentenaz.Forge.Super)";

#[allow(clippy::disallowed_methods)]
fn curl(args: &[&str], body: Option<&str>) -> Result<String, String> {
    let mut child = Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "60", "--user-agent", USER_AGENT])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if let (Some(body), Some(mut stdin)) = (body, child.stdin.take()) {
        stdin
            .write_all(body.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    let output = child.wait_with_output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("request failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let directory = args.next().unwrap_or_else(|| ".".to_string());
    let crate_name = args.next();

    let workspace = load_workspace(&directory)?;
    let krate = match &crate_name {
        Some(name) => workspace
            .find(name)
            .ok_or_else(|| format!("no crate named {name:?} in this workspace"))?,
        None => workspace
            .default_crate()
            .ok_or_else(|| "the workspace has no crates".to_string())?,
    };
    let lockfile = read_lockfile(&workspace.root)?;
    let reachable = lockfile.reachable_crates_io_packages(&krate.name, &krate.version);
    println!(
        "crate: {} {} ({} crates.io packages reachable)",
        krate.name,
        krate.version,
        reachable.len()
    );

    let batches = osv_batches(&reachable);
    let mut hits = Vec::new();
    for batch in &batches {
        let answer = curl(
            &["--request", "POST", "--data-binary", "@-", OSV_BATCH_URL],
            Some(&batch.body),
        )?;
        hits.extend(batch.parse_response(&answer)?);
    }

    let ids = advisory_ids(&hits);
    println!(
        "{} batch request(s); {} packages with advisories; {} records to fetch",
        batches.len(),
        hits.len(),
        ids.len()
    );

    let mut records: Vec<AdvisoryRecord> = Vec::new();
    for group in ids.chunks(8) {
        let fetched: Vec<Result<AdvisoryRecord, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = group
                .iter()
                .map(|id| {
                    scope.spawn(move || {
                        curl(&[&advisory_url(id)], None).and_then(|body| parse_advisory(&body))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_else(|_| Err("thread panicked".into())))
                .collect()
        });
        for (id, result) in group.iter().zip(fetched) {
            match result {
                Ok(record) => records.push(record),
                Err(error) => println!("  {id}: details unavailable: {error}"),
            }
        }
    }

    let findings = merge_findings(&hits, &records);
    let mut heading = None;
    for finding in &findings {
        if heading != Some(finding.kind) {
            heading = Some(finding.kind);
            println!(
                "\n{}",
                match finding.kind {
                    FindingKind::Vulnerability => "VULNERABILITIES",
                    FindingKind::Unsound => "UNSOUND",
                    FindingKind::Unmaintained => "UNMAINTAINED",
                    FindingKind::Notice => "NOTICES",
                }
            );
        }
        println!(
            "  {:<9} {:<22} {:<10} {}",
            // A severity only means something for a vulnerability; a notice
            // without one is not "unknown", it just has none.
            match (finding.severity, finding.kind) {
                (Some(severity), _) => severity.label(),
                (None, FindingKind::Vulnerability) => "unknown",
                (None, _) => "",
            },
            finding.package,
            finding.version,
            finding.id
        );
        if let Some(summary) = &finding.summary {
            println!("            {summary}");
        }
        if !finding.fixed_in.is_empty() {
            println!("            fixed in {}", finding.fixed_in.join(", "));
        }
        if finding.details_missing {
            println!("            details could not be fetched");
        }
    }

    let counts = FindingCounts::of(&findings);
    println!(
        "\n{} vulnerabilities, {} unsound, {} unmaintained, {} other notices",
        counts.vulnerabilities, counts.unsound, counts.unmaintained, counts.notices
    );
    if hits.iter().any(|hit| hit.truncated) {
        println!("some packages had more advisories than were returned; the list is incomplete");
    }
    Ok(())
}
