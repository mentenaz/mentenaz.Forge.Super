# Fork checks: spec

Two checks that protect the fork's branding and naming after every upstream Zed merge. This spec defines behaviour, config formats and acceptance tests. It deliberately does not fix the implementation details that depend on the repo (field names in `editions.json`, exact file paths). Those are marked **(discover)** and the implementer should read the repo and confirm them.

## Goals

- Catch new user-visible upstream "Zed" text that arrives through a merge without any conflict or compile error.
- Catch hand-copied names (installer, packaging, workflows) drifting away from `editions.json`.
- Keep the output short enough to read after a large upstream pull.

## Non-goals

- Auto-fixing anything. The checks report; a person decides.
- Making packaging read `editions.json` directly. That is a separate, later change.
- Touching licence or attribution text. The fork is GPL-3.0, and credit to Zed Industries must stay.

## Implementation notes

- **Where the code lives: a standalone crate, `tooling/fork_checks`, with its own `[workspace]`.** It is Rust, so it runs the same on Windows and Linux and needs no Node. It is deliberately not a subcommand of upstream's `xtask`, and not a member of the root workspace.
  - **Why.** Both of those mean editing upstream-owned files. The `tooling/*` entries in the root `Cargo.toml` `members` list changed in the last merge (`crates_io_client` was added), and `tooling/xtask` has changed upstream at least three times since September. Every such edit is a merge conflict surface for a tool whose job is to protect merges.
  - **Precedent.** `tooling/lints` is already its own workspace, so a nested `[workspace]` needs no edit to the root `Cargo.toml`.
  - **Build cost.** It builds without gpui or any of the Zed workspace, so it takes seconds and CI can run it before the big build.
  - **Cost of the choice.** It has its own `Cargo.lock`, which is committed. Dependency versions are chosen to match the root's pins (`regex`, `globset`, `toml`, `serde`, `serde_json`, `anyhow`, `clap`), but they are not shared with the workspace lock.
- **Subcommands:** `scan` (Check A, with `--pre` or `--post`) and `names` (Check B).
- **How it is run.** There is no cargo alias, because `.cargo/config.toml` is upstream's file and one added line there is another conflict surface. The long form is `cargo run -j 8 --release --manifest-path tooling/fork_checks/Cargo.toml -- scan --post`. A thin wrapper, `script/fork-check.ps1`, gives the short form and passes `-j 8`; it follows `script/test-fork-crates.ps1`, which is the existing precedent for fork scripts.
- **Tests.** The acceptance tests run as the crate's own tests, against fixture repos and diffs built in temp directories: `cargo test -j 8 --manifest-path tooling/fork_checks/Cargo.toml`. `script/test-fork-crates.ps1` only covers packages of the main workspace, so the CI workflow that runs the checks must also run this test command.
- **Toolchain.** Nothing to configure: the root `rust-toolchain.toml` applies to a nested crate. (`tooling/lints` has its own file only because it needs nightly.)
- **Lints.** A standalone workspace does not inherit the root's `[workspace.lints]`, but the repo's `clippy.toml` may still apply, and it can forbid calls such as spawning processes. The checker runs `git`, so if CI lints this crate, follow the build scripts' pattern: an `#[allow(clippy::disallowed_methods)]` with a reason.
- **No edits to upstream-owned files.** Not the root `Cargo.toml`, not `.cargo/config.toml`, not `tooling/xtask`. Everything new is ours: `tooling/fork_checks/**`, `script/fork-check.ps1`, `.forge/fork-checks/**` and the workflow file.
- **Config lives in `.forge/fork-checks/`.** `.forge/context/` is git-ignored but `.forge/fork-checks/` is not (checked with `git check-ignore`), so the config is tracked. `.forge/` is also where the app keeps per-project data, so keep everything of ours under `fork-checks/`.
- Normalise path separators and line endings before comparing, so Windows and Linux runs agree.

---

## Check A: upstream-literal scan

### Purpose

List new lines that an upstream merge brings into our tree which contain user-visible Zed branding, so each one gets a decision.

### Input: two modes

**`--post`: what the merge actually changed in our tree.** This is the mode that gates. It scans the diff from our side of the merge to the merged result:

- HEAD is a merge commit: `HEAD^1..HEAD`.
- A merge is in progress (`MERGE_HEAD` exists, for example after `git merge --no-commit`): `HEAD` against the working tree.
- Anything else: an explicit `--range <a>..<b>` is required. Without it the tool stops with an error and never guesses a range.

**`--pre`: the early look.** Informational only. It scans upstream's range before anything is merged: `<merge base of HEAD and the upstream revision>..<the upstream revision>`. The upstream revision is `--upstream <rev>`, which takes any commit-ish: a remote-tracking branch, a tag or a commit. The default is `upstream/main`, so a local run after `git fetch upstream` needs no argument. It shows what is coming, but it includes lines that conflict resolution, or our own code, will change.

Why two: the dry run on the merge to dev-v50 (41 upstream commits, 10,480 added lines) shows the difference.

- `--pre` reported `starts_with("zed://")` in `windows_only_instance.rs`. The merge resolved that line to `client::is_app_url(...)`, so it is not in our tree.
- `--post` did not report it. Outside test code, `--pre` had 2 hits and `--post` had 1 (a user-agent string in the new `crates_io_client` tooling).

Both modes:

- Only **added lines** (`+` lines of `git diff --unified=0`) are examined. Unchanged code is never scanned, so a line already in our first parent is never reported, even if it contains `Zed`.
- The same patterns, exclusions and allowlist apply.
- **The scanned revision** is the end of the scanned range, and it is where file contents are read (to find test ranges) and where `git blame` runs (to name the commit behind each hit):
  - `--post` on a committed merge: `HEAD`.
  - `--post` on a merge in progress: the working tree.
  - `--pre`: the upstream revision.
  - `--post --range <a>..<b>`: `<b>`.
- A custom range is written `<a>..<b>`. The three-dot form `<a>...<b>` is rejected with an error, because it means a different diff (from the merge base) and silently accepting it would scan the wrong lines.
- The report header names the scanned revision.

### Scope (v1)

- **File types scanned:** `*.rs` and `assets/**/*.json` (default settings, keymaps and the like). In JSON, `//` comments are comments, not text.
- **"Inside a string literal"** is decided by the same single lexer pass over the file that finds the test ranges (see "Detecting test code"). It classifies every position as code, comment or string, including multi-line strings, raw strings (`r#"..."#`) and JSON strings. So a match is judged by where it falls in the file, not by whether the added diff line happens to contain a quote.
- **Not scanned yet:** every other file type, and user-visible text that lives outside Rust and JSON. See "Deferred".

### What counts as a hit

An added line is a hit when it contains, inside a **string literal**, any pattern from `patterns` below. Patterns are configurable, with these defaults:

| Pattern | Meaning |
| --- | --- |
| `\bZed\b` (case-sensitive) | the product name in text |
| `zed://` | the URL scheme |
| `zed.dev` | upstream's hosted services and docs |
| `zed-industries` | upstream's organisation name |
| `dev\.zed\.` | bundle and app identifiers |
| `(?i)(\.config\|\.local[\\/]share\|\.cache\|\.var[\\/]app)[\\/]zed\b` | Linux config, data and cache folders (`~/.config/zed`) |
| `(?i)(AppData[\\/](Local\|Roaming\|LocalLow)\|Application Support\|Library[\\/](Logs\|Caches\|Preferences))[\\/]zed\b` | Windows and macOS folders (`AppData\Local\Zed`, `Application Support/Zed`) |
| `(?i)%(APPDATA\|LOCALAPPDATA)%[\\/]zed\b\|\$\{?XDG_[A-Z_]+_HOME\}?[\\/]zed\b` | the same places written with variables (`%APPDATA%\Zed`, `$XDG_CONFIG_HOME/zed`) |

Patterns use the Rust `regex` syntax: no look-around and no backreferences, which is why the path patterns end in `\b` and not a look-ahead. Paths are matched with either slash.

**Why the path patterns are narrow.** An earlier draft used one broad pattern, `(?i)[\\/](\.config[\\/])?zed[\\/]`, which matches any `/zed/` path segment. Run over the non-test Rust code in `crates/`, it matched 16 existing strings, all of them source-tree paths such as `crates/zed/Cargo.toml` in build scripts, which cannot be renamed and are not user-visible. The three narrow patterns match none of those. Upstream documents its folders in doc comments (`paths.rs`), which are excluded, so the narrow patterns are written from those documented locations and checked with positive and negative examples (test 38), and not from existing literals.

Identifiers are **not** hits: `ZedFoo`, `zed_extension_api` and `use zed::...` are code, not text a user sees.

### Exclusions (never reported)

- Comments and doc comments.
- Test code, as defined in "Detecting test code" below.
- Licence and attribution material. These are **protected**: the tool must never flag them for replacement. See "Protected paths" below.
- `docs/`, `CHANGELOG*`, and `*.md` unless configured otherwise.
- Anything matching the allowlist (below).

### Detecting test code

A diff alone cannot say whether an added line sits inside a test module, so the tool reads each changed `.rs` file at the scanned revision (see "Input: two modes"), works out which line ranges are test code, and maps the added line numbers from the diff hunk headers onto them. This is a heuristic, not a parser: it needs no dependencies and is meant to be easy to read.

A line is test code when it is in one of these:

1. **A whole test file:** under a `tests/` directory, or named `tests.rs`, `*_tests.rs` or `test_*.rs`.
2. **A test module:** `#[cfg(test)]` followed (skipping other attributes and blank lines) by `mod <name> {`. The range runs from the attribute line to the matching closing brace.
3. **A single test item:** `#[cfg(test)]` followed by something other than a module (`fn`, `impl`, `use`, `const`). The range is that one item: to its closing brace, or to its `;` for a one-line item.
4. **A test function outside a test module:** a function with `#[test]`, `#[gpui::test]` or `#[tokio::test]` (any `#[<path>::test]`), with the same range rule as item 3.

Brace matching skips braces inside string literals (including raw strings), character literals and comments, so a test that contains `"{"` or `'}'` does not end the range early or late.

**Why not "from `#[cfg(test)]` to the end of the file".** A survey of `crates/` and `tooling/` (a rough one, with naive brace counting, so the second figure is an upper bound):

- 969 files have an inline test module. In 879 of them (91%) it is the last thing in the file; in about 52 (5%) real code follows it.
- 264 uses of `#[cfg(test)]` are on a single item (`fn`, `use`, `impl`), not a module.
- 49 are external modules (`mod tests;`), which rule 1 covers by file name.

An end-of-file rule would hide any hit in the code that follows a test module or a single test item. A missed hit is the harmful direction for this check, so the heuristic is built to err the other way.

**Fail safe.** If a range cannot be determined (the braces do not balance, or the item cannot be recognised), nothing is excluded for that file: its hits are reported, and the report notes the file as "test range unclear". A spurious hit costs a decision; a missed one costs the branding.

### Allowlist format

File: `.forge/fork-checks/zed-literals.toml`. It holds both kinds of entry for Check A: `[[allow]]` (below) and `[[ignore]]` (see "Ignored paths").

```toml
[[allow]]
id = "telemetry-docs-link"
path = "crates/client/src/**"          # glob
literal = "https://zed.dev/docs/telemetry"   # the whole string literal, or use regex = "..."
reason = "Docs link kept on purpose until we host our own page"
decided = "2026-11-09"
expires = "2027-02-01"                  # optional: forces a re-check
```

Rules:

- Match on **path glob + the string literal's text**, never on line number (line numbers drift on every merge).
- `path` is required. Use `**` to mean any file; an entry with no `path` is a config error, so a repo-wide allowance is always a visible choice.
- Exactly one of `literal` or `regex` is required; both or neither is a config error.
- `reason` and `decided` are required. `expires` is optional; an expired entry is reported as NEW again.
- An entry that no longer matches anything is reported as STALE so the file doesn't collect dead entries.

#### What an entry matches

A hit is one **string literal** that contains a pattern, and an entry is matched against that literal as a whole.

- **`literal`** must equal the literal's complete text. `literal = "Zed"` does not allow `"Open Zed settings"`. This is deliberate: if upstream changes the wording even slightly, the entry stops matching and the hit comes back as NEW for a fresh decision, instead of staying allowed by accident.
- **`regex`** is searched within the literal's text, so it matches anywhere unless anchored. For a whole-literal match, anchor it: `^Open Zed .*$`. Use it when the text varies, for example a version number.
- **The text is the string's value, not its source form.** Escapes are processed first: the Rust literal `"Say \"Zed\""`, the raw string `r#"Say "Zed""#` and the JSON string `"Say \"Zed\""` all have the text `Say "Zed"`, and one entry matches all of them. Only the common escapes need handling (`\"`, `\\`, `\n`, `\t`, and a trailing `\` that continues a line); an escape the tool does not know is left as written.
- **One literal at a time.** The scan lexes literals; it does not evaluate `format!`, `concat!` or string concatenation. A message built from two literals is two literals, each judged on its own. If one line holds two literals that both contain a pattern, an entry allows only the one it matches, and the other stays NEW.
- **Copy and paste.** The report shows each NEW hit's literal text exactly as `literal` must equal it, so writing the entry is a copy and paste.

### Ignored paths

Some upstream code is never shipped, and a whole new crate of it would otherwise flood NEW with hits that need no decision. Those paths are listed in the same file, as `[[ignore]]` entries:

```toml
[[ignore]]
path = "tooling/**"                    # glob
reason = "Upstream build and dev tooling. None of it is bundled into the app: bundle-windows.ps1, bundle-linux and bundle-deb never mention it."
decided = "2026-11-09"
```

- `reason` and `decided` are required. An entry without a reason is a config error.
- An ignore is for a **whole path**; an allowlist entry is for one **literal**. Use the narrowest one that fits.
- Nothing ignored is silent: the report shows a count of hits per ignore entry (group 4 below).
- An ignore entry that matches no changed file in the run is not reported as stale: unlike an allowlist entry, it is expected to match nothing most of the time.
- The initial config has just the one entry above. Add others (for example server-only crates that are never shipped) only when real hits show up, with the same kind of reason.

### Protected paths

Some "Zed" text is there on purpose: licence texts and credit to Zed Industries. The fork is GPL-3.0 and that credit must stay. A hit in such a file must never be offered for replacement, so those paths are protected.

**Built in, and not removable.** The tool itself protects, whatever the config says:

- files named `LICENSE*` or `NOTICE*`, and paths under `assets/licenses*`;
- any file whose first 20 lines contain "Zed Industries" together with "Copyright" or "SPDX-License-Identifier" (case-insensitive). This is a heuristic for attribution headers.

There is no setting that turns these off. This is the floor the "never touch licence or attribution text" non-goal rests on.

**Added in the config.** For anything else, `zed-literals.toml` takes `[[protect]]` entries:

```toml
[[protect]]
path = "crates/zed/resources/credits/**"          # glob
reason = "Credits page: names Zed Industries and other contributors"
decided = "2026-11-09"
```

- `reason` and `decided` are required. An entry without a reason is a config error.
- A hit in a protected file is not reported as NEW, and is never offered for replacement.
- Protected is not the same as ignored. An `[[ignore]]` is for code that does not ship, where no decision is needed. A `[[protect]]` is for text that ships and must keep its "Zed" wording. The report counts them separately.
- It is not the same as an allowlist entry either: an `[[allow]]` means we looked at one literal and chose to keep it for now, and it can expire. Protection has no expiry.
- Like `[[ignore]]`, a `[[protect]]` entry that matches nothing in a run is not reported as stale.
- The initial config has no `[[protect]]` entries. The built-in floor covers the known cases.

### Output

A Markdown report, written **in full** to a file and **capped** to stdout (see "Size of the report"). Its header names the mode and the exact range or merge that was scanned (for `--pre`, the upstream revision as it was given and as a commit hash); in `--pre` mode it says the hits are before resolution. It has four groups:

1. **NEW**: hits that need a decision. For each: file, added text, the matched pattern, and the commit that introduced it (see "Attribution").
2. **ALLOWED**: a count per allowlist `id`, not a full listing.
3. **STALE**: allowlist entries that matched nothing, and entries that have expired.
4. **IGNORED AND PROTECTED**: a count of hits per `[[ignore]]` entry and per `[[protect]]` entry (including the built-in floor), listed separately. Nothing skipped is silent.

Exit codes: `0` when NEW is empty; `1` when NEW is non-empty in `--gate` mode. In `--report` mode it always exits `0`. `--post` defaults to `--gate` and `--pre` to `--report`; either default can be overridden.

#### Size of the report

The first goal is output short enough to read after a large pull, and a single new upstream file can bring hundreds of hits. So what is printed is capped, and what is saved is not.

**stdout (and the CI job summary)**

1. One verdict line first: `N new strings in M files to review`, or `No new Zed text.` when NEW is empty.
2. The header: mode, range or merge, and the counts for every group (NEW hits and files, ALLOWED, STALE, IGNORED and PROTECTED).
3. NEW grouped by file, in path order, which is the same on every run and so is easy to compare. Only the first **5 files** are printed in full; after them one line says `and K more files (M hits); full list in <report file>`.
4. Within a file, only the first **10 hits** are printed, then `and K more in this file`.
5. Hits in one file with identical literal text are shown once, with a count and the first few line numbers (`same text x40, lines 12, 18, 31, ...`), so a string repeated in a new file is one decision and not forty.

The two numbers are the flags `--max-files` (default 5) and `--max-hits-per-file` (default 10). `0` means no cap.

**The report file** has everything, with the same grouping and no cap.

**Why the job summary is the capped view.** GitHub limits a step summary to 1 MiB, and an uncapped report from a big merge could exceed it. The CI job writes the capped view to the summary and uploads the full report as the artifact.

**The cap is display only.** It never changes the counts in the header or the exit code. A run with `--max-files 1` fails or passes exactly as one with no cap, and the header still says how many files and hits there are.

#### Attribution

Each hit names the commit that introduced it, found with `git blame --porcelain` at the scanned revision (see "Input: two modes"; for a custom range that is the end of the range, `<b>`). The report shows the short hash and the subject line.

- This reports the line as it is in the scanned result. Walking the range commit by commit would also report lines that a later commit in the same range removed or changed.
- When the blamed commit is the merge commit itself, the line was written or resolved by us in the merge. The report marks it "resolved in the merge". That is a useful signal on its own: it is where a resolution replaced upstream's text.
- Checked on the dev-v50 merge: a line from upstream blames to the upstream commit (`ccb25cbc63`), and the resolved `is_app_url` line blames to the merge commit (`fbb662635f`).
- **(discover)** Not yet checked: `git blame` on a working-tree file while a merge is in progress. If it does not follow `MERGE_HEAD`, fall back to `git log -S` over the range for those lines.

### Decision workflow for each NEW hit

1. Replace with the constant from `app_names`, **or**
2. add an allowlist entry with a reason, **or**
3. it is attribution or a licence text: add a `[[protect]]` entry (a path and a reason). The built-in floor already covers `LICENSE*`, `NOTICE*`, `assets/licenses*` and files with a Zed Industries copyright header.

---

## Check B: name consistency

### Purpose

Fail loudly when a hand-copied name in packaging or workflows no longer matches `editions.json`, whether an upstream merge or one of our own edits changed it. A second kind of target does the same for a value that has no catalog entry but must be identical in several files (the apt signing-key fingerprint).

### Source of truth

`editions.json` only. The checker holds **no copy of any name**; it reads the values at run time.

### Targets file

File: `.forge/fork-checks/name-targets.toml`

```toml
# Which catalog editions the packaging files are written for. Every edition in
# editions.json must be in exactly one of these two lists, so adding an
# edition to the catalog forces a decision here.
shipped_editions   = ["forge-super"]
unshipped_editions = ["forge-zed", "forge-lite"]

# Plain form: literal text, with catalog fields as {placeholders}.
[[target]]
id = "windows-bundle-exe-name"
file = "script/bundle-windows.ps1"
# editions = ["forge-super"]               # optional; defaults to shipped_editions
expect = '-Destination "$innoDir\{install_name}.exe"'

[[target]]
id = "deb-package-name"
file = "script/bundle-deb"
expect = 'pkg_name="{command}${{suffix}}"'   # {{ and }} are literal braces

# Regex form, for when plain text is not enough (here: two alternatives, both required).
[[target]]
id = "windows-bundle-cli-name"
file = "script/bundle-windows.ps1"
must_match = '\\bin\\{command}(\.exe)?"'     # {placeholders} are filled in and regex-escaped
min_count = 2
```

Each target has **exactly one** of `expect` or `must_match`. Both or neither is a config error.

**`expect` (the plain form, used wherever possible).**

- It is literal text. The only special syntax is `{name}`, which is replaced by that catalog field's value for the edition being checked (`{command}`, `{install_name}`, `{display}`, `{id}`, `{ipc}`, `{scheme}`, `{package_identity}`). One `expect` can use several placeholders.
- `{{` and `}}` stand for literal braces, so a shell variable such as `${suffix}` is written `${{suffix}}`.
- A `{name}` that is not a field of the catalog is a config error, not a silent literal. A typo therefore cannot turn into a target that quietly never matches.
- It is matched as a substring of one line. Runs of spaces and tabs in `expect` match any run of spaces and tabs in the file, so re-aligning `$appName     = "..."` does not break a target.
- Everything else, including `$`, `\`, quotes and dots, is literal. There is no escaping to get wrong.

**`must_match` (the regex form).** For the few cases that need alternatives or repetition. `{name}` placeholders work here too and are filled in with the value regex-escaped; the rest of the pattern is a regular expression and is the author's to escape. In a regex, braces that do not hold a plain name (`{2}`, `{1,3}`) are regex quantifiers, not placeholders; `{word}` that is not a catalog field is still a config error.

**Both forms.** `min_count` (default `1`) is how many lines must match. The test is "at least `min_count` matching lines", not an exact count.

#### Shared values: `[[same]]`

Some hand-copied values are not in the catalog but must be identical in several files. A `[[same]]` entry checks that, without any catalog field:

```toml
[[same]]
id = "apt-signing-key-fingerprint"
files = ["script/install.sh",
         ".github/workflows/build_installers.yml",
         ".github/workflows/promote-apt-stable.yml"]
capture = '(?:EXPECTED_KEY_FINGERPRINT|GPG_FPR)="([0-9A-F]{40})"'
```

- `capture` is a regex with **exactly one capture group**. The group is the value.
- Every listed file must have at least one match, and **all captured values, across all files, must be identical** (exact, case-sensitive). Otherwise it fails, naming each file and the value found in it, or the file that has no match.
- It runs once. It is not per edition, and `shipped_editions` does not apply.
- It checks that the files **agree**, not that the value is **right**. A change made consistently in all the files passes, which is what a deliberate key rotation looks like. The checker holds no copy of the value, in keeping with "holds no copy of any name".
- Config errors: fewer than two files; the same file listed twice; a `capture` with no capture group or more than one.

### Targets to cover (v1)

All for the shipped edition (`forge-super`). Catalog fields are the ones in `editions.json`: `command`, `install_name`, `display`, `id`, `ipc`, `scheme`, and `package_identity` (see below). Each target is written as an anchored `expect` (or, where that is not enough, a `must_match`). Anchored means the text includes the surrounding syntax, such as the variable name and the quotes, and not just the value, so that a partial revert is caught.

| File | Must contain | Catalog fields (placeholders) |
| --- | --- | --- |
| `script/bundle-windows.ps1` | the exe copied to `$innoDir\{install_name}.exe` | `install_name` |
| `script/bundle-windows.ps1` | the cli moved to `bin\{command}.exe` and to `bin\{command}` | `command` |
| `script/bundle-windows.ps1`, stable block | `$appName` and `$appDisplayName` equal `"{display}"` | `display` |
| | `$appUserId` equals `"{id}"` | `id` |
| | `$appMutex` equals `"{ipc}-Stable-Instance-Mutex"` | `ipc` |
| | `$appExeName` and `$regValueName` equal `"{install_name}"` | `install_name` |
| | `"UrlScheme" = "{scheme}"` | `scheme` |
| `crates/explorer_command_injector/AppxManifest.xml` | `Name="{package_identity}"`, `Executable="{install_name}.exe"`, `DisplayName` equals `{display}` | `package_identity`, `install_name`, `display` |
| `AppxManifest-Preview.xml` and `-Nightly.xml` | the same three, with `.Preview` / `.Nightly` after the identity and ` Preview` / ` Nightly` after the display name | same |
| `crates/zed/resources/windows/zed.sh` | `$ZED_PATH/{command}.exe` (twice) | `command` |
| `script/bundle-deb` | `pkg_name="{command}${suffix}"` and `Description: {display} code editor` | `command`, `display` |
| `script/bundle-linux` | `{command}$suffix.app`, `libexec/{command}-editor`, `APP_CLI="{command}"`, `APP_ICON="{command}"`, and `APP_ID="{id}"` in the stable block | `command`, `id` |
| `script/install.sh` | `PACKAGE_NAME="{command}"` | `command` |
| `script/publish-apt-repo` | repository names `{command}-$distribution` | `command` |
| `.github/workflows/build_installers.yml` | artifact names `{command}-linux-...` and the release title `{display} ${RELEASE_TAG}` | `command`, `display` |

Shared values (`[[same]]`, no catalog field):

| Files | Value |
| --- | --- |
| `script/install.sh` (`EXPECTED_KEY_FINGERPRINT`), `.github/workflows/build_installers.yml` and `.github/workflows/promote-apt-stable.yml` (both `GPG_FPR`) | the apt signing-key fingerprint. If one file differed, signing and trust would disagree and `apt update` would fail for every user. Key rotation will change it, so a missed file is a plausible mistake. |

`promote-apt-stable.yml` has no edition names (it hands the repository build to `script/publish-apt-repo`), so the fingerprint is the one hand-copied value in it that is worth checking.

Notes:

- **Which files a merge can change.** Upstream has `bundle-windows.ps1`, `bundle-linux`, `install.sh` (upstream's own installer, which the fork replaced with the apt installer), `zed.sh`, `zed.iss` and the three Appx manifests, so an upstream merge can change or conflict with those. Upstream does not have `bundle-deb`, `publish-apt-repo`, `build_installers.yml` or `promote-apt-stable.yml`; for those, the checks guard against our own edits and not against merges. Both groups are worth checking, but the first group is where merges can cause damage.
- **Channel blocks.** `bundle-windows.ps1` and `bundle-linux` have one block per channel (stable, preview, nightly, dev), and the other blocks carry suffixed values such as `dev.forge.super-Preview`. v1 anchors the stable block only, using the closing quote so that `"dev.forge.super"` does not match `"dev.forge.super-Preview"`.
- **`package_identity`.** The Appx package identity (`Mentenaz.Forge.Super`, the same name as the winget package) had no field in the catalog, so it was added to `editions.json`. It is optional in the catalog: only an edition that ships through Appx needs it, and only `forge-super` has it. The Rust build ignores the field. If a shipped edition lacks it, the Appx targets are a config error (see test 43).
- **Field mapping.** For `forge-super`, `dir` and `install_name` are both `ForgeSuper`, so `$regValueName` would pass with either. It is mapped to `install_name`. Revisit if the two ever diverge for a shipped edition.
- **`zed.iss` is not a target.** It holds no names; it takes `{#AppName}`, `{#AppExeName}` and the rest as defines set in `bundle-windows.ps1`, which is covered above.
- **Left out for now:** `script/run-isolated.ps1`, `script/install-local.ps1` and `.github/workflows/build_windows_dev.yml`. They use `ForgeSuper` for dev-build data folders and carry no installer names.

### Behaviour

- A `[[target]]` runs once for each edition in its `editions` list, which defaults to `shipped_editions`. Editions in `unshipped_editions` are never checked. A `[[same]]` entry runs once.
- Why the split: the packaging files hold only the shipped edition's values (`CLAUDE.md` says the installer side repeats the `forge-super` values by hand). Running every target against every catalog edition would fail all of them on day one.
- Config errors (never a pass): an edition named in `shipped_editions`, `unshipped_editions` or a target's `editions` that is not in the catalog; a catalog edition that is in neither list, or in both; a target whose `editions` names an edition that is not in `shipped_editions`.
- A target **fails** when the file is missing, or fewer than `min_count` lines match its `expect` or `must_match`.
- There is no `forbid` in this version. See "Deferred" below.
- A target whose `expect` or `must_match` uses a `{placeholder}` that is not a catalog field, or names a field that a listed edition lacks, is a **config error**, not a pass.

### Output

`names` has no `--gate` or `--report` modes (those belong to `scan`). One line per `[[target]]` and per `[[same]]`: `PASS` or `FAIL` with the file, the edition (for a `[[target]]`), and what was expected, or the files and the differing values (for a `[[same]]`). A summary line at the end. Exit `1` if any target fails or has a config error.

---

## Wiring

| Where | Which check | Mode |
| --- | --- | --- |
| Every push to `main` and every pull request (CI, no path filter) | B (`names`), and the crate's own tests | gate: a red run |
| Manual dispatch of `fork_checks.yml` (CI) | A | `--pre`, report: summary and artifact |
| Local, before a manual merge | A | `--pre`, report |
| Local, after a manual merge | A and B | A in `--post` and `--gate` mode; B always fails on a problem |

Check A has no schedule. Without an upstream-sync job there is nothing new to scan between merges, so it runs when a merge is being prepared (CI dispatch or local) and after it (local).

### The CI workflow: `.github/workflows/fork_checks.yml`

A new file, ours. It follows `fork_tests.yml`: `permissions: contents: read`; a concurrency group per workflow and ref with `cancel-in-progress`; `actions/checkout@v4`; `Swatinem/rust-cache@v2` with `workspaces: "tooling/fork_checks -> target"`; `CARGO_INCREMENTAL: 0`; a timeout of 15 minutes; and `-j 4`, because the hosted runner has 4 cores (the 8 jobs used locally are sized for a developer machine).

Triggers: `push` to `main`, `pull_request`, and `workflow_dispatch` with one input, `upstream_ref`: the name of a **branch or tag on the upstream repository**, default `main` (for example `main`, or `v1.28.0` to scan against a specific upstream release).

**No path filter** on push and pull request. The job is cheap, and a path list would duplicate the files in `name-targets.toml`: a target file added there but missing from the filter would never be checked, and nothing would say so.

Jobs:

1. **`tests`**: a matrix of `windows-latest` and `ubuntu-latest`, running `cargo test` on the crate. Both systems, because acceptance test 44 requires the same results on both.
2. **`names`**: `ubuntu-latest`, running `names`. On push, pull request and dispatch. `names` has no modes: it always fails the run when a target fails or the config is wrong.
3. **`scan_pre`**: `ubuntu-latest`, **only on `workflow_dispatch`**.
   - Checks out with `fetch-depth: 0` (the merge base and `git blame` need history).
   - Adds the public remote `https://github.com/zed-industries/zed.git` as `upstream` (no token needed) and runs `git fetch upstream "$UPSTREAM_REF"`. That works for a branch or a tag name alike, and leaves the commit in `FETCH_HEAD`.
   - Runs `scan --pre --report --upstream <the resolved commit hash>`, writes the capped view to the job summary, and uploads the full report as the artifact `fork-checks-scan-report`. Passing the hash means the scan never depends on which kind of ref the name was.
   - The input reaches the shell through an environment variable (`UPSTREAM_REF: ${{ inputs.upstream_ref }}`), never by interpolating it into the script, and is checked first against `^[A-Za-z0-9._/-]+$`. This is the pattern `promote-apt-stable.yml` already uses for its `tag` input.
   - If the name does not exist on the upstream repository, the fetch fails and the job fails with that error. It never falls back to another ref.

Gate and reports:

- A failing `names` makes the run red. A push to `main` cannot be blocked, so, as with `fork_tests.yml`, a red run on `main` is a signal to fix forward. For pull requests the check can be made required through branch protection, which is a repository setting and not part of this spec.
- Reports go to the job summary and an artifact. There are no issue or pull request comments, so the workflow needs only read access.
- Check A `--post` is not in CI: it needs a merge in progress, so it runs locally.

---

## Acceptance tests

Use small fixture repos or fixture diffs, not the real Zed history.

**Check A**

1. A diff adding `"Open Zed settings"` in a UI file produces one NEW hit.
2. A diff adding `// Zed does X` produces no hit.
3. A diff adding `use zed::Foo;` or a type named `ZedFoo` produces no hit.
4. A diff adding a hit in `LICENSE-GPL` or an attribution header produces no hit and is never listed as replaceable.
5. A hit that matches an allowlist entry is counted under ALLOWED, not NEW.
6. An allowlist entry past its `expires` date reports its matches as NEW.
7. An allowlist entry that matches nothing is listed as STALE.
8. Changing only a line number (moving code) does not turn an ALLOWED hit into NEW.
9. An upstream line that the merge resolves to a constant is reported by `--pre` and not by `--post`.
10. With a merge in progress (`MERGE_HEAD` present), `--post` scans `HEAD` against the working tree, not upstream's range.
11. `--post` when HEAD is not a merge commit and no `--range` is given is an error that asks for `--range`.
12. A line already in our first parent that contains `Zed` is never reported by `--post`.
13. A hit inside a `#[cfg(test)] mod tests { ... }` produces no hit.
14. A hit in non-test code that comes after a `#[cfg(test)]` module in the same file is reported. (This is the case an end-of-file rule would miss.)
15. A hit inside a single `#[cfg(test)] fn` or `use` produces no hit, and the next item in the file is still scanned.
16. Braces inside string literals, raw strings, character literals and comments within a test module (`"{"`, `'}'`, `// }`) do not end the test range early or late.
17. A file whose test range cannot be determined (unbalanced braces) reports its hits and is listed as "test range unclear".
18. A function with `#[test]`, `#[gpui::test]` or `#[tokio::test]` outside a `#[cfg(test)]` module is treated as test code.
19. A hit in a file under `tests/`, or named `tests.rs`, `foo_tests.rs` or `test_foo.rs`, produces no hit.
20. A hit on a line in the middle of a multi-line string literal (the line itself has no quote) is reported.
21. A hit inside a raw string (`r#"Open Zed"#`) is reported.
22. A hit under an `[[ignore]]` path is not in NEW, and is counted under IGNORED against that entry.
23. An `[[ignore]]` entry without a `reason` is a config error.
24. A NEW hit names the commit that introduced it; a line written in the merge itself is marked "resolved in the merge".
25. In `--pre`, a line that one commit adds and a later commit in the range changes is attributed to the later commit, and a line that a later commit removes is not reported at all.
26. `--pre --upstream <rev>` scans from the merge base of HEAD and `<rev>` to `<rev>`, and gives the same result whether `<rev>` is a branch, a tag or a commit hash that points at the same commit. With no `--upstream` it uses `upstream/main`.
27. A hit in a path matched by a `[[protect]]` entry is not in NEW, is counted under PROTECTED against that entry, and is not counted under IGNORED.
28. A `[[protect]]` entry without a `reason` is a config error.
29. With no `[[protect]]` entries at all, a hit in `LICENSE-GPL`, in `NOTICE`, under `assets/licenses*`, or in a file whose first lines carry a Zed Industries copyright header is still protected and never in NEW.
30. `literal` must equal the whole string literal: an entry `literal = "Zed"` does not allow `"Open Zed settings"`, which stays NEW.
31. A `regex` entry is searched within the literal's text: `Open Zed` allows `"Open Zed settings"`, and `^Open Zed$` does not.
32. Matching uses the unescaped value: one entry for the text `Say "Zed"` allows the Rust literal `"Say \"Zed\""`, the raw string `r#"Say "Zed""#` and the JSON string `"Say \"Zed\""`.
33. When one line holds two string literals that both contain a pattern, an entry allows only the literal it matches; the other stays NEW.
34. An `[[allow]]` entry with no `path`, or with both or neither of `literal` and `regex`, is a config error.
35. With more NEW files than `--max-files`, stdout lists the first N files in path order and ends with `and K more files (M hits)`; the report file lists every file.
36. A file with more hits than `--max-hits-per-file` shows that many and `and K more in this file`; identical literal text in one file is shown once with a count.
37. The cap changes only the display: with `--max-files 1` the header counts and the exit code are the same as with no cap, and `--max-files 0` prints everything.
38. The path patterns hit `~/.config/zed/settings.json`, `$XDG_CONFIG_HOME/zed/themes`, `${XDG_DATA_HOME}/zed`, `%APPDATA%\Zed\settings.json`, `C:\Users\x\AppData\Local\Zed\db`, `~/Library/Application Support/Zed`, `~/Library/Logs/Zed/Zed.log`, `~/.local/share/zed/extensions` and `~/.cache/zed`; and they do not hit `crates/zed/Cargo.toml`, `../zed/Cargo.toml`, `zed_extension_api`, `~/.config/zedfoo` or `~/.config/zed_server`.
39. With `--post --range <a>..<b>`, the scanned diff is `<a>..<b>`, and file contents (for test ranges) and `git blame` come from `<b>`: a change made after `<b>` does not affect the result. The three-dot form `<a>...<b>` is rejected with an error.

**Check B**

40. Passing case: every target matches the catalog for the shipped editions and the run exits `0`.
41. Reverting the binary name in `bundle-windows.ps1` to the upstream name fails that target.
42. Renaming an edition's `command` in `editions.json` without updating a packaging file fails that target.
43. A target whose `expect` or `must_match` uses a placeholder for a field that does not exist is a config error with a clear message. This includes a shipped edition that lacks the optional `package_identity` when an Appx target needs it.
44. The same inputs give the same results on Windows and Linux (path separators, line endings).
45. An edition in `unshipped_editions` is not checked: its values are absent from the packaging files and the run still exits `0`.
46. A new edition added to the catalog but listed in neither `shipped_editions` nor `unshipped_editions` is a config error naming that edition.
47. An edition named in either list, or in a target's `editions`, that does not exist in the catalog is a config error.
48. A target's `editions` overrides the default: with `editions = ["forge-super"]` it is checked once, and with an edition that is not shipped it is a config error.
49. `expect` ignores the width of whitespace runs: re-aligning `$appName     = "..."` does not fail the target.
50. A target with both `expect` and `must_match`, or with neither, is a config error.
51. `{{` and `}}` in `expect` match literal braces (`${{suffix}}` matches `${suffix}`), and an unescaped `{suffix}` that is not a catalog field is a config error.
52. An `expect` with two placeholders (`{ipc}-Stable-Instance-Mutex` style) fills both from the edition and matches only the full text.
53. `must_match` with `min_count = 2` fails when only one of the two lines is present.
54. `[[same]]`: when every listed file captures the same value, the entry passes.
55. `[[same]]`: when one file captures a different value, the entry fails and names each file with its value.
56. `[[same]]`: a listed file with no match fails the entry and is named.
57. `[[same]]` config errors: one file only, the same file twice, and a `capture` with zero or two groups are each a config error.

## Deliverables

What exists when this work is finished. Only the first row exists today.

| # | Deliverable | Path | State |
| --- | --- | --- | --- |
| 1 | The catalog field `package_identity` | `editions.json` | done, not committed |
| 2 | This spec | `docs/fork-checks-spec.md` | written, not committed |
| 3 | The checker crate: `scan` and `names`, with the acceptance tests as its own tests | `tooling/fork_checks/` (own `[workspace]`, own `Cargo.lock`) | to build |
| 4 | The wrapper script | `script/fork-check.ps1` | to build |
| 5 | The CI workflow | `.github/workflows/fork_checks.yml` | to build |
| 6 | Check B config | `.forge/fork-checks/name-targets.toml` (initial content below) | to write |
| 7 | Check A config | `.forge/fork-checks/zed-literals.toml` (initial content below) | to write |

### Initial `name-targets.toml`

Written from the table in "Targets to cover (v1)". It was run through a simulator of the rules in this spec against the real files in the dev-v50 tree, and **all 30 targets and the one `[[same]]` entry pass today**, so a first run of the real checker should be green. If it is not, either the checker or this file is wrong, and that is worth finding out before relying on either.

```toml
# Which catalog editions the packaging files are written for.
shipped_editions   = ["forge-super"]
unshipped_editions = ["forge-zed", "forge-lite"]

# --- script/bundle-windows.ps1
[[target]]
id = "windows-exe-name"
file = "script/bundle-windows.ps1"
expect = '-Destination "$innoDir\{install_name}.exe"'

[[target]]
id = "windows-cli-name"
file = "script/bundle-windows.ps1"
must_match = '\\bin\\{command}(\.exe)?"'
min_count = 2

[[target]]
id = "windows-app-name"
file = "script/bundle-windows.ps1"
expect = '$appName = "{display}"'

[[target]]
id = "windows-app-display-name"
file = "script/bundle-windows.ps1"
expect = '$appDisplayName = "{display}"'

[[target]]
id = "windows-app-user-id"
file = "script/bundle-windows.ps1"
expect = '$appUserId = "{id}"'

[[target]]
id = "windows-app-mutex"
file = "script/bundle-windows.ps1"
expect = '$appMutex = "{ipc}-Stable-Instance-Mutex"'

[[target]]
id = "windows-app-exe-name"
file = "script/bundle-windows.ps1"
expect = '$appExeName = "{install_name}"'

[[target]]
id = "windows-reg-value-name"
file = "script/bundle-windows.ps1"
expect = '$regValueName = "{install_name}"'

[[target]]
id = "windows-url-scheme"
file = "script/bundle-windows.ps1"
expect = '"UrlScheme" = "{scheme}"'

# --- crates/explorer_command_injector/AppxManifest.xml
[[target]]
id = "appx-stable-identity"
file = "crates/explorer_command_injector/AppxManifest.xml"
expect = 'Name="{package_identity}"'

[[target]]
id = "appx-stable-executable"
file = "crates/explorer_command_injector/AppxManifest.xml"
expect = 'Executable="{install_name}.exe"'

[[target]]
id = "appx-stable-display-name"
file = "crates/explorer_command_injector/AppxManifest.xml"
expect = '<DisplayName>{display}</DisplayName>'

# --- crates/explorer_command_injector/AppxManifest-Preview.xml
[[target]]
id = "appx-preview-identity"
file = "crates/explorer_command_injector/AppxManifest-Preview.xml"
expect = 'Name="{package_identity}.Preview"'

[[target]]
id = "appx-preview-executable"
file = "crates/explorer_command_injector/AppxManifest-Preview.xml"
expect = 'Executable="{install_name}.exe"'

[[target]]
id = "appx-preview-display-name"
file = "crates/explorer_command_injector/AppxManifest-Preview.xml"
expect = '<DisplayName>{display} Preview</DisplayName>'

# --- crates/explorer_command_injector/AppxManifest-Nightly.xml
[[target]]
id = "appx-nightly-identity"
file = "crates/explorer_command_injector/AppxManifest-Nightly.xml"
expect = 'Name="{package_identity}.Nightly"'

[[target]]
id = "appx-nightly-executable"
file = "crates/explorer_command_injector/AppxManifest-Nightly.xml"
expect = 'Executable="{install_name}.exe"'

[[target]]
id = "appx-nightly-display-name"
file = "crates/explorer_command_injector/AppxManifest-Nightly.xml"
expect = '<DisplayName>{display} Nightly</DisplayName>'

# --- crates/zed/resources/windows/zed.sh
[[target]]
id = "windows-wrapper-script"
file = "crates/zed/resources/windows/zed.sh"
expect = '"$ZED_PATH/{command}.exe"'
min_count = 2

# --- script/bundle-deb
[[target]]
id = "deb-package-name"
file = "script/bundle-deb"
expect = 'pkg_name="{command}${{suffix}}"'

[[target]]
id = "deb-description"
file = "script/bundle-deb"
expect = 'Description: {display} code editor'

# --- script/bundle-linux
[[target]]
id = "linux-app-dir"
file = "script/bundle-linux"
expect = '/{command}$suffix.app'

[[target]]
id = "linux-editor-binary"
file = "script/bundle-linux"
expect = 'libexec/{command}-editor'

[[target]]
id = "linux-app-cli"
file = "script/bundle-linux"
expect = 'export APP_CLI="{command}"'

[[target]]
id = "linux-app-icon"
file = "script/bundle-linux"
expect = 'export APP_ICON="{command}"'

[[target]]
id = "linux-app-id"
file = "script/bundle-linux"
expect = 'APP_ID="{id}"'

# --- script/install.sh
[[target]]
id = "install-package-name"
file = "script/install.sh"
expect = 'PACKAGE_NAME="{command}"'

# --- script/publish-apt-repo
[[target]]
id = "apt-repo-names"
file = "script/publish-apt-repo"
expect = '"{command}-$distribution"'
min_count = 3

# --- .github/workflows/build_installers.yml
[[target]]
id = "release-artifact-names"
file = ".github/workflows/build_installers.yml"
expect = 'name: {command}-linux-'
min_count = 2

[[target]]
id = "release-title"
file = ".github/workflows/build_installers.yml"
expect = '--title "{display} '

[[same]]
id = "apt-signing-key-fingerprint"
files = ["script/install.sh",
         ".github/workflows/build_installers.yml",
         ".github/workflows/promote-apt-stable.yml"]
capture = '(?:EXPECTED_KEY_FINGERPRINT|GPG_FPR)="([0-9A-F]{40})"'
```

### Initial `zed-literals.toml`

```toml
# Check A config. The patterns, the exclusions and the protected licence and
# attribution paths are built into the checker (see the spec).

[[ignore]]
path = "tooling/**"
reason = "Upstream build and dev tooling. None of it is bundled into the app: bundle-windows.ps1, bundle-linux and bundle-deb never mention it."
decided = "2026-11-09"
```

There are no `[[allow]]` and no `[[protect]]` entries to start with. They are added when real hits call for them, each with a reason.

### First real-world check: replay the dev-v50 merge

Before trusting the scan, replay the merge it was designed around. Both runs use a custom range, which scans a fixed piece of history regardless of where `HEAD` is today.

| Command | Expected |
| --- | --- |
| `scan --post --range 504abf602a..145bcfc1bc` (what upstream sent: 41 commits) | **1 NEW**: `starts_with("zed://")` in `crates/zed/src/zed/windows_only_instance.rs`. The other hits in this range are in test code (excluded) or under `tooling/**` (ignored, counted). |
| `scan --post --range fbb662635f^1..fbb662635f` (what the merge landed in our tree) | **0 NEW**. The `zed://` line is gone, because the merge resolved it to `client::is_app_url`. The user-agent string in the new `crates_io_client` is counted as IGNORED. |

The expectations come from the measurements in this spec (7 hits before resolution and 6 after; 4 of them in a test file, 1 in a `#[cfg(test)]` module, 1 in tooling). If the real checker disagrees, find out which side is wrong.

### Suggested build order

1. Commit the spec and `editions.json`. Write the two config files.
2. The crate skeleton and **`names`** first: config parsing, `expect`, `must_match`, `[[same]]`, the config errors, and tests 40 to 57. It is the smaller check and the quickest to be useful. Run it on the real repo: it should be green.
3. The wrapper script and `fork_checks.yml` with the `tests` and `names` jobs, so Check B is running in CI early.
4. **`scan`**, in this order: the lexer (strings, comments, test ranges), then hit detection with the patterns and exclusions, then the allow, ignore and protect tables, then output and the size cap, then attribution.
5. The dev-v50 replay above.
6. The `scan_pre` dispatch job in the workflow.
7. Before relying on attribution in a real merge: check `git blame` on a merge in progress (the one open "discover" item), and use the fallback if it does not follow `MERGE_HEAD`.

## Deferred

- **`forbid` (patterns that must not appear in a packaging file).** Left out of this version. The files legitimately contain upstream names that cannot change: `bundle-windows.ps1` alone has 12 bare uses of `zed` (the `crates/zed` folder, `--package zed`, artifact names, the Sentry project). A broad pattern would fail on day one, and a narrow one is a guess until there is real drift to learn from.
  - Revisit when a merge reverts a packaging name that the positive checks did not catch. Then add a narrow `forbid` for that string (for example an upstream executable name) on the one file where it happened.
  - The positive checks (`expect` / `must_match`) already catch a reverted name in the places we list, so this loses little.
- **Built-artefact checks.** Reading the installer's output filenames, or the package name from the built `.deb` (`dpkg-deb --field Package`), would catch more than the source checks, but only after a full build, which takes hours. Revisit if a source check ever passes while a built installer is wrongly named.
- **Patterns for the project folder `.zed/` and the server folders `.zed_server`, `.zed_wsl_server`.** Not default patterns. The fork still uses these names (the Rust code has 5 existing `.zed/...` literals, such as `.zed/settings.json`, and 2 for the server folders), so flagging new ones would mostly mean allowlist work. Decide once the fork has a position on renaming them; the check can then get a pattern for them.
- **Other file types for Check A.** v1 scans `*.rs` and `assets/**/*.json` only. Not covered: TOML, PowerShell, YAML, and user-visible text in Markdown under `assets/` or in `crates/**/resources/`. Add them when a real "Zed" string turns up in one of them.

## Open questions

- **Check A on every push to `main`, in report mode (proposed, not decided).** For a push that fast-forwards an upstream merge, the range `${{ github.event.before }}..${{ github.sha }}` is the same change `--post` would scan, so every merge would leave a report in the run summary without anyone having to remember to run it. It would need `--post --range` and a full-history checkout on push runs.
- **Should `names` be a required check on pull requests?** That is a branch protection setting, outside this repository's files.

Settled and no longer open: the checks run from a new `fork_checks.yml` and not from an upstream-sync job (there is none); Check A has no schedule; reports go to a job summary and an artifact; Check B checks source files only.
