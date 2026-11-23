# npm_bootstrap

Offers to run `npm install` when you open a JavaScript project whose
dependencies have not been installed yet.

## Why it exists

A freshly cloned or unzipped Node project does nothing useful until
`npm install` has run, and it is easy to forget. This crate notices that
state and offers the fix in one click.

It hooks the generic `Workspace` lifecycle rather than any particular "open"
command, so it applies however the folder was opened — drag and drop,
File → Open Folder, or a clone from the Helm panel.

## What you see

When a workspace opens and its first worktree root has a `package.json` but
no `node_modules`, a toast appears:

> package.json found — install dependencies?  **Run npm install**

Clicking the button runs `npm install` in that folder in the Script Runner, which
is revealed so the install can be watched (it can take minutes). The Script Runner
asks the trust gate, so nothing runs in a folder that is not trusted.

`npm install` runs the project's own `preinstall` and `postinstall` scripts, so the
offer is **not made while the project is in Restricted Mode**. It is made when the
project becomes trusted. A project made by the Scaffold panel with "Run install
afterwards" turned off is marked as already offered, and gets no offer.

The offer is made **once per project directory**. The marker is written as
soon as the toast is shown, so dismissing it does not bring it back next
time.

## Behaviour details

- Only the workspace's first worktree is checked.
- If the worktree is not attached yet when the workspace is created, the
  check waits for the first `WorktreeAdded` event.
- The "already asked" marker is stored in Zed's key-value store under
  `npm-bootstrap-asked:<absolute path>`.
- It always runs `npm` (`npm.cmd` on Windows). Other package managers are not
  detected here; use the Node Package Manager tab
  ([`npm_manager_panel`](../npm_manager_panel/README.md)) for those.
- Install output is not streamed. For live output run the install from the
  Node panel instead.

## How it is wired

One call, `npm_bootstrap::init(cx)`, from `fn main()` in
`crates/zed/src/main.rs`. There is no UI, action or setting of its own.

## Development

```sh
cargo check -p npm_bootstrap -j 8
cargo test -p npm_bootstrap -j 8
```

The tests cover when the install offer appears and the once-per-project key.

To run the tests of every fork crate at once: `script/test-fork-crates.ps1`.
