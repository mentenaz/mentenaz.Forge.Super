# Scaffold panel: plan

Status: **phases 1 to 5 are built and merged, and phase 6 is mostly done** (see "Where it stands" just below). The plan
below was written on 22 November 2026 from reading the engine (`E:\mentenaz\Forge.Scaffold.SDK`: `Overview.md`,
`docs/integration-notes.md`, `docs/roadmap.md`, and the public API in `src/lib.rs`, `src/plan.rs`, `src/fetch.rs`,
`src/store.rs`, `src/manifest.rs`) and the templates repo (`E:\mentenaz\Forge.Scaffold.Templates`), and is kept as it was
written. Things not read or not checked are in "Not verified" at the end, not hidden.

## Where it stands (23 November 2026)

| Phase | State |
|---|---|
| 1 panel, `Fetch`, generator list | Done |
| 2 the form from the manifest | Done. List fields are rows with a [-] and a [+] each, and every field is validated as you type, by the engine's own rules (solution name, package name, web part names, duplicates ignoring case). "Next" waits while a field is wrong |
| 3 review: file tree, warnings | Done |
| 4 location and create | Done: folder picker (remembers the last one), plan at the real location, progress bar, the engine removes a half-written folder, "Open project" in a new window |
| 5 install behind the gate | Done, as the existing install offer in `npm_bootstrap`: not made until the gate allows the folder, runs in the Script Runner (which asks the gate), a spinner toast, and an automatic start when "Run install afterwards" is on. Found on the way: that offer used to run `npm install` with no trust check |
| 6 index-driven list | Mostly done. Search, tag filter and tabs (All, Installed, Local); "Update available" on a row and an "Update this template" button in the form; source labels (Microsoft SPFx, create-vite, Forge) and a **Legacy** tag with a notice in the form; a clear message when a template needs a newer Forge Super. **Open:** auto-update (decided: off, manual only for now) and a `source` field in the index so the labels are read, not inferred |
| 7 existing solution: info, add, edit, undo | Not started. Idea so far: a gear in the Node panel's SPFx section opens a short form (name, description, version, which is all the engine lets you edit) in a tab, with a preview of the changes before applying and an undo afterwards; "add a component" is a separate action |
| 8 analysis review | Blocked on the analysis engine |
| 9 more generators, "save as template" | The full-stack, node API and Vite generators are in the list but have **not been tried in the panel**; "save as template" is not started |

Not in the plan, built along the way: the search box and tag filter, the tabs, and remembering the chosen tag, tab and
last location in the Forge database.

## What this is

A panel in this fork that creates new projects from the Forge scaffolding engine: pick a generator and version, answer the
manifest's questions, review exactly what will be written, choose a folder, create it, and carry on to an install. It also
shows an existing SPFx solution and can add a component, edit it, or undo an edit.

The engine does the work and draws nothing. It runs no commands and makes no network calls of its own. The panel supplies
the screen, the HTTP fetch and the process runner.

## Rules the panel must follow (from `CLAUDE.md`)

1. **Built from `gpui_component` widgets**, not hand-rolled `div` lists: a resizable split, `Stepper`, `Select`, `Input`,
   `Switch`, `Alert`, `Tree`, `Progress`, `Spinner`, tinted `Tag`. A panel that only looks like these is a failed port.
   Conventions: `crates/gpui_component/PORTING.md` (crate layout, theme bridging, the checklist for the next port).
2. **No blocking work on the main thread.** `plan()`, `apply()`, the index fetch, archive downloads and the store scans all go
   in `cx.background_spawn`, hopping back with `this.update` only to store the result. While the first result is pending,
   show a loading row (`Spinner` plus a label), never an empty state.
3. **Project tooling only in a trusted project.** The install step runs `npm` (or another package manager) in the new
   folder, through the Script Runner, which asks `forge_trust_gate`. A new folder is Restricted until trusted, so the flow
   ends in the usual "Trust and Continue". The panel itself starts no program of its own. A new `Command::new` needs its own
   gate check.
4. **Every root fills the dock** (`.size_full()`), including loading and error placeholders.
5. **Tinted status tags**, not filled variants (`Tag::custom(color.opacity(0.2), color, color.opacity(0.3))`), and icons from
   `crates/gpui_component_assets/assets/icons/` (add an SVG, `IconName` is generated from the file name).
6. Names and strings: "Mentenaz Forge Super" in anything shown to the user; no hard-coded product names (`editions.json`).

## Architecture

- **New crate `crates/scaffold_panel`**, registered like the other Forge panels: workspace member and
  `[workspace.dependencies]` in the root `Cargo.toml`, a dependency of `crates/zed`, loaded in `zed.rs` (see how
  `ScriptRunnerPanel::load` is wired), and added to the dock's panel list in `workspace/src/dock.rs`. A panel that runs
  tooling on its own would also go in `PANELS_NEEDING_TRUST`; this one does not scan, so it probably does not.
- **The engine as a pinned git dependency** on `https://github.com/mentenaz/Forge.Scaffold.SDK` at a commit (the repo is
  public, Apache-2.0). Same approach as the fnm fork. Not a path dependency, which would not build for anyone else.
- **`Fetch` implemented with the fork's HTTP client** (the engine's trait is `get(url, etag) -> FetchResult`). The engine
  verifies SHA-256 and extracts archives safely itself.
- **The templates folder** comes from the engine's `resolve_templates_root`, kept in the app's data folder (not in a
  project).
- **State** lives on the panel: the store handle, the loaded version lists, the current wizard step and answers, the plan
  once made, progress, and the last error. Each long operation has a "current" token so a late answer for something the
  user has since changed is dropped (the publish dialog's picker does the same).

## Screens

### 1. Generators (left side of a resizable split)

| Element | Widget | Source |
|---|---|---|
| list of generators, grouped, with a search box | `searchable_list` or `Tree`, `Input` | store: installed, remote index, custom folders |
| per-row badge | tinted `Tag` | Installed, Update available (revision went up), New (not downloaded), Custom, Needs newer Forge |
| "Check for updates", auto-update switch | `Button`, `Switch` | `check_updates`, `UpdateSettings`, `auto_update` |
| loading and offline state | `Spinner`, `Alert` | the list works offline from what is installed |

### 2. New project (main area, a `Stepper` wizard)

1. **Choose:** generator and version (`Select`; newest first, from `list_versions`). Shows the Node requirement
   (`check_node` result) as an `Alert` before anything else.
2. **Options:** the form, generated from the manifest's prompts. Text becomes `Input` (masked when the prompt says so),
   Bool becomes `Switch`, Choice becomes `Select`, List becomes a repeatable row. Validation messages come from the
   engine's strict name rules, shown under the field.
3. **Review:** the plan. A `Tree` of every file that would be written, the warnings (nested solution, long Windows path,
   untested package manager, install scripts, URL or git dependencies) as `Alert`s, and the post steps listed. Nothing is
   written yet.
4. **Location:** folder picker; shows the final path `[location]/[name]` and refuses an existing folder.
5. **Create:** `apply()` with its progress callback driving a `Progress` bar; a failed write removes the folder it made.
6. **Done:** open the folder, then the post steps (see Install below), then the analysis review when it exists.

### 3. Existing solution (a second mode of the same area)

Uses `find_spfx_solutions` and `solution_info` to show the solution and its components; actions: add a component
(`plan_add` shown as a diff of the four config files, then `apply_add`), edit name, description and version (`plan_edit`,
`apply_edit`), and undo the last change (`revert_edit`, from the undo record). Every write is shown before it happens.

## Template sources and updates

The list is **whatever the index offers**, not a fixed set, so a template added to the templates repo and published reaches
users without a Forge release. Merging a pull request changes nothing for users: only publishing a new `index.json`
does, and each entry pins the archive's SHA-256. That publish step is the trust boundary.

- Labels: **Official** (built from Microsoft's MIT-licensed `SharePoint/spfx`, carries a `NOTICE.txt`), **Legacy SPFx
  (generator-derived)** for 1.20.0 to 1.23.2, **Custom** (a folder the user made; never overwritten), **Community** if that
  ever exists.
- The legacy label links to the notice in the templates repo's README and `LICENSING.md`: built from `yo`, modified, and
  not a permission from Microsoft (discussion 11052 is unanswered). The panel must not describe those as licensed.
- A template that needs something the panel lacks (a newer manifest `schema`, a prompt type, a post action) shows
  **Needs newer Forge** instead of failing half way.

## Install, trust and the analysis review

The engine returns post steps as descriptors (`install` with a package manager, and others), with an absolute folder. The
panel never runs them itself:

1. After Create, offer **Open project**. The new folder opens in Restricted Mode like any unknown folder.
2. After **Trust and Continue**, the Script Runner runs the install; the gate allows it because the folder is trusted.
3. Where it exists, the **analysis review** (`forge-analysis <folder> --brief`) goes between Create and Install: what it saw
   (install-time scripts, where dependencies come from, effects by kind) and what it could not see. It never says "safe";
   "incomplete" is the normal result. It is not built yet and the engine's `TODO(forge-analysis)` marks show where it plugs
   in.
4. Offer "install without scripts" (`--ignore-scripts`) as the cautious choice.

## Build phases

| Phase | Deliverable | How to check |
|---|---|---|
| 1 | Crate, git dependency, `Fetch`, panel in the dock listing generators and versions from the local store, offline | the list shows with no network; `cargo check -p scaffold_panel -j 8` |
| 2 | The Options form for one generator (SPFx 1.23.2), validation | wrong names refused with the engine's message |
| 3 | Review: file `Tree`, warnings, Node check | nothing written to disk at this step |
| 4 | Location, Create, progress, failure cleanup | create, then break the write and see the folder removed |
| 5 | Post steps through the Script Runner, with the trust step | an untrusted new folder: install refused with the trust message; after trust: runs |
| 6 | Index-driven list: Check for updates, badges, Needs newer Forge, source labels | publish a test index with one new entry |
| 7 | Existing solution: info, add, edit, undo | undo restores the files |
| 8 | Analysis review, once that engine is confirmed | a hostile control project is flagged |
| 9 | Later: Vite and full-stack generators (the templates repo already has `fullstack` and `backend-api` folders), "save this project as a template" | |

Each phase is its own branch off `main`, built with `-j 8`, tried with `script/run-isolated.ps1`, and merged only after the
user has looked at it.

## Decisions needed

1. **Index display fields.** An index entry has only a generator id, version, revision, URL, hash and size; a template that
   is not downloaded has no title, description or tags to show. Adding optional fields is a schema bump (the index rejects
   unknown fields). Best done before phase 6.
2. **Where the wizard lives:** a dock panel with the form in a workspace tab (what you first described), or one wizard
   dialog like the Publish dialog. This plan assumes the dock list plus a workspace tab.
3. **First generators:** SPFx only, or SPFx and Vite React.
4. **Legacy generator-derived versions:** shown by default with the notice, or hidden until the question is answered.
5. **Where the templates folder lives**, and whether auto-update is on by default. *Location decided (phase 1):* in the
   app's data folder beside the Node and Python version stores (`paths::data_dir()/scaffold_templates`, so an isolated build
   keeps its own); the engine's `FORGE_TEMPLATES` variable still overrides it. A setting for the location is deferred.
   Auto-update is still open.
6. **Panel name and icon** (an SVG to add).

*Answers so far:* (1) done, the index is schema 2 with titles and tags, and a `source` field is still wanted; (2) a dock
list plus a workspace tab, as built; (3) both SPFx and Vite are in the list; (4) legacy versions stay visible, labelled
Legacy, with the notice; (5) the location is decided (above) and auto-update is **off** for now; (6) still the placeholder
box icon, no keybinding yet.

## Not verified

- The exact API of `gpui_component`'s `Stepper`, `Tree` and `Progress`, and how a workspace tab item is built in this
  fork. Confirm in phase 1 before committing to the layout.
- Which HTTP client crate the fork exposes to panels for `Fetch`.
- How much the new crate adds to build time (measure with `script/measure-build-timings.ps1`, which is not committed).
- The remaining engine source (`add.rs`, `edit.rs`, `undo.rs`, `check.rs` and the binary) was not read, only its public
  surface; the post-step action list was not read in full.
- The SPFx versions other than 1.23.2 were not checked for what Microsoft-named text they still carry.

## Out of scope

Running the real `yo` generator, the optional NuGet source, template authoring tools in the panel, and anything that makes
the engine call the network or run a command itself.
