.rules

# Cockpit panel ports: `gpui_component` is the requirement, not an option

Panels ported from the Forge repo (`E:\Forge_GPUI`) into this tree
(`cockpit_panel`, `script_runner_panel`, `processes_panel`, `python_panel`,
`node_panel`, `npm_manager_panel`) are expected to be built on
**`gpui_component`'s actual widget and layout system** —
`gpui_component::setting::{Settings, SettingPage, SettingGroup, SettingItem}`,
`h_resizable`/`resizable_panel`, `gpui_component::text::markdown`,
`Input`/`InputState`, etc. A port that renders flat, hand-rolled `div` lists
instead of these widgets is a **failed port**, even if it compiles and works.

The first version of `npm_manager_panel` shipped exactly that way (a
hand-rolled three-tab list, no `setting::`, no resizable split, no registry
search). The approved remedy lives in
`crates/gpui_component/NPM_MANAGER_PANEL_UI_RESTORE_PLAN.md` — read it before
touching the npm manager, and follow `crates/gpui_component/PORTING.md` for the
general porting conventions (crate layout, `AppState`-stripping, theme
bridging). Before declaring any such port done, confirm the UI is actually
composed from `gpui_component` widgets, not just `div`s that resemble them.

# Panels: keep blocking work off the main thread

`cx.spawn` runs on the foreground (main) thread. Anything slow inside it —
a `sysinfo` scan (`System::new_all`, `refresh_processes`), a `cargo`/`npm`/
`dotnet` subprocess, a directory walk — stalls the whole UI, and at startup
it delays every other panel, including terminal restoration. Put that work in
`cx.background_spawn` and hop back with `this.update` only to store the
result. A poll loop must do this on every tick, not just the first.

While the first result is pending, show a loading row (`Spinner` plus a short
label) instead of an empty or "nothing found" state, the way the terminal
panel shows "Restoring terminals…".

To check, read the dev build's log for `New foreground hang detected` entries:
each lists the `file:line` and duration of a main-thread stall. `Background
hang` entries are off-thread and do not freeze the UI.

# Panels: every root fills the dock

The dock draws a panel through a cached view, so a root element with no
explicit width shrinks to its content. Every root a panel can return from
`render` — including the early-return placeholder for loading, "not found"
and error states — needs `.size_full()` (or `.w_full().h_full()`).

# Icons

`gpui_component::IconName` is generated from the file names in
`crates/gpui_component_assets/assets/icons/` (`home.svg` → `IconName::Home`,
`Forge_Cockpit2.svg` → `IconName::ForgeCockpit2`). To add an icon, add the SVG
there. Icons are drawn as a mask in the current text colour, so the SVG's own
fill colour does not matter.

Filled `Tag` variants (`Tag::warning()` and friends) are unreadable in themes
where the fill and its foreground are close. For status badges use the tinted
form, `Tag::custom(color.opacity(0.2), color, color.opacity(0.3))`, or
`.outline()`.

# Building, running and logs

- Always pass `-j 8` to cargo on this machine.
- For a manual test run use `script/run-isolated.ps1`, which builds and runs
  the app as a separate "ForgeSuperBuild" with its own data directory and
  instance lock. A plain `cargo run` shares both with an installed Forge and
  will not start while it is running.
- A cargo command left running holds the target-directory lock and blocks any
  other build, so do not leave one in the background while someone else builds.
- A crate's package name can differ from its directory
  (`crates/gpui_component_fps` is package `gpui-fps`); check its `Cargo.toml`
  before `cargo -p`.
- The data and log folder is `%LOCALAPPDATA%\<dir>`, where `<dir>` is the
  edition's `dir` from `editions.json` plus the channel. On the `stable`
  channel the installed app and a plain `target/debug/forgedev.exe` both log
  to `%LOCALAPPDATA%\ForgeSuper\logs\ForgeSuper.log`; `run-isolated.ps1` builds
  log to `%LOCALAPPDATA%\ForgeSuperBuild\logs\ForgeSuperBuild.log`. A build
  launched from a terminal logs to that terminal instead.

# App names and channel

- This app is the `forge-super` edition (Mentenaz Forge Super, winget
  `Mentenaz.Forge.Super`, URL scheme `forgesuper://`). Product names
  (display name, data folder, exe and CLI names, mutex, app id, URL scheme)
  come from `editions.json`, turned into constants by
  `crates/app_names/build.rs`. Change names there, not in code.
- The installer side does not read `editions.json` yet: `bundle-windows.ps1`,
  `zed.iss`, `zed.sh` and the `AppxManifest*.xml` files repeat the
  `forge-super` values by hand, and their comments say what each must match.
- Only `forgesuper://` is registered with the OS, but `zed://` links are still
  accepted and open the same thing (`client::normalize_app_url`). Internal
  `zed:///agent/…` and `zed-cli://` addresses stay as upstream has them.
- Keychain entries are stored as `forgesuper:<url>`, apart from Zed's.
  Entries saved under the bare URL by earlier builds are copied across on
  first read (`zed_credentials_provider`).
- Settings and data from the fork's earlier builds (`ZedDev` folders) are
  copied in by `crates/zed/src/zed/import_previous_data.rs`: automatically
  on the first launch, and through the "Import Data From Previous Version"
  command (which restarts and imports before anything opens the database).
  It never touches upstream Zed's `Zed` folders.
- The channel is the one word in `crates/zed/RELEASE_CHANNEL` (now `stable`).
  `ZED_RELEASE_CHANNEL` overrides it at build time; nothing else does.
- The built-in updater is always off, in every build (`update_explanation`
  in `crates/auto_update`), because it would fetch upstream Zed from
  zed.dev. Updates come from winget and the `.deb`; "Check for Updates" only
  says so. `ZED_UPDATE_EXPLANATION` changes that message.

# Releases

A release is an annotated tag `dev-vN` on `main`, pushed to `origin`
(`mentenaz/mentenaz.Forge.Super`). The tag push runs the `build_installers` workflow,
which publishes a normal (not pre-) release "Mentenaz Forge Super dev-vN
(x86_64)" with `Mentenaz-Forge-Super-x86_64.exe`. Use the next number after
the latest `dev-v*` tag, and a message of the form
`dev-vN: comma-separated summary of what changed since the last tag`.
`gh` resolves to upstream Zed by default here; pass `-R mentenaz/mentenaz.Forge.Super`.

- The `N` in the tag is the fourth part of the installer version
  (`1.27.0.N`), which winget and Windows compare, so tag numbers must only
  go up. `RELEASE_VERSION` stays the crate version.
- Tag builds are Windows-only until the Linux packaging (`bundle-linux`,
  `bundle-deb`) is renamed to Forge Super; macOS is off because webrtc-sys
  fails to build there. Both can still be run manually from the Actions tab,
  and manual runs publish a "dev-build" pre-release.
- A new tag push cancels a `build_installers` run still in progress, so
  wait for it to finish first. Pushes to `main` alone do not start a build.
- Pushing needs the `mentenaz` GitHub account active (`gh auth switch -u
  mentenaz`); the work account gets a 403.
