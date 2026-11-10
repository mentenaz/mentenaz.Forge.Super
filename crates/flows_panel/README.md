# flows_panel

A left-dock panel listing every workflow (`.flow.json`) in the open
workspace, with buttons to create, open, run and stop them, and the links of
the servers a running flow has started.

## Why it exists

Flows are files, and files get lost in a tree. This panel is the index: one
place that shows which workflows a project has, when each last ran and how it
went, and that can start one without opening it first.

Ported from Forge, limited to the `.flow.json` format. Forge's older
`.fdgn`/`.fwrk` ForgeFlow format was never brought into this tree.

## Using it

Open with `ctrl-k f` (`cmd-k f` on macOS), the status bar icon (tooltip
"Flows"), or `flows panel: toggle focus`.

### Toolbar

| Button         | Does                                                                 |
| -------------- | -------------------------------------------------------------------- |
| Refresh        | Rescan the workflows directory                                       |
| New Workflow   | Create a blank `.flow.json` and open it                              |
| Add Task Chain | Scan the workspace for services and generate a flow that starts them |

### Each row

| Button | Does                                                                              |
| ------ | --------------------------------------------------------------------------------- |
| Run    | Run the flow now; the row shows the outcome of the last run                       |
| Stop   | Replaces Run (with a spinner) while the flow is running: stops it and its servers |
| Open   | Open the flow through the normal file-open path (disabled while it runs)          |
| Graph  | Open the flow on the Designer canvas directly (disabled while it runs)            |

Both Open and Graph end up on the Designer canvas: the Designer claims every
`.flow.json` file, so the normal file-open path leads there too. To see the
raw JSON, use the **Raw** button in the Designer's toolbar.

Rows are sorted by name. A row's tag says how the last run ended: **last run
ok**, **last run failed** or **last run stopped**. A stopped run is not
counted as a failure.

### Running servers

A flow that starts servers keeps running for as long as they do, so its row
shows Stop until you press it. While it runs, a line appears under the row
for each server that is up and has an address:

```
● Start backend:   http://localhost:8000    [open] [copy]
● Start frontend:  http://127.0.0.1:5173/   [open] [copy]
```

**Open** launches the address in the browser and **Copy** puts it on the
clipboard. The address comes from the node's `url`, else an address the
server printed, else its `ready_url`, else `localhost:` and its `ready_port`
(copy only). A server with none of these is not listed. The lines go away
when the run ends. Running the same flow again first stops the servers its
earlier run left, so there is one server per port.

Closing the panel stops the runs it started. For per-node colours and the
run log, run the flow from the Designer instead.

### Add Task Chain

The wizard lists the services detected in the workspace. For each one you can
adjust the Name, Command, Args, Working Dir and Port, tick or untick it, or
remove it. **Add Custom Service** adds a blank row. **Create** writes a new
flow that starts the selected services; **Cancel** discards it.

Detection looks at the workspace root and its immediate subfolders for Node
(`package.json`: React/Vite/Next frontends, Express/Fastify/Koa/Nest
backends, other apps) and Python (Django, FastAPI, Flask, or a `main.py`,
`app.py` or `server.py`) projects. Ports are the usual defaults for each kind
(5173, 8000, 5000), not read from the project's own settings, so check them.
A Python project that has a `.venv` or `venv` folder, in its own folder or in
the workspace root, is started with that environment's interpreter (FastAPI
and Flask run as `python -m uvicorn` and `python -m flask`), so its installed
packages are found.

The flow has, for each service, a `StartProcess` and a `WaitForPort`, and the
services start in parallel. The `StartProcess` has `url` and `ready_port` set
from the Port, so it turns Ready once the server answers on that port. If a
server uses a different port from the one the wizard guessed, edit the Port
before you create the flow.

The Node panel's **Task Chain** quick action opens this same wizard anchored
to that project.

## Where flows are stored

In `.zed/workflows` under the workspace root by default. The directory is
created on first use. Change it in `settings.json`:

```json
{
  "flows": {
    "workflows_dir": "automation/flows"
  }
}
```

The path is relative to the workspace root.

## How it is wired

- `flows_panel::init(cx)` registers `flows_panel::ToggleFocus`.
- `FlowsPanel::load(workspace, cx)` is awaited in `initialize_panels`
  (`crates/zed/src/zed.rs`), which also hands a `WeakEntity<FlowsPanel>` to
  the Node panel.
- Implements `workspace::dock::Panel`, docked left by default (the
  `flows_panel.dock` setting moves it left or right; `flows_panel.button` hides
  its status bar button), `activation_priority() = 28`.
- `open_task_chain_wizard_anchored(...)` is the public entry point other
  panels use to open the wizard for a specific project.

## Layout

| File                    | Contents                                          |
| ----------------------- | ------------------------------------------------- |
| `src/flows_panel.rs`    | Panel, flow list, task-chain wizard, run handling |
| `src/flows_settings.rs` | The `"flows"` settings key (`FlowsSettings`)      |
| `src/persistence.rs`    | Per-project run history                           |

Running a flow calls `workflow_engine::run_workflow_controlled` with a
`RunControl` kept per flow (`run_controls`). Stop calls `RunControl::stop`,
the server lines read `RunControl::service_links`, and `Drop for FlowsPanel`
stops every live run. The engine's status callback is not a UI event, so
while a flow runs the panel repaints every half second. The result is
recorded when the run ends. History is kept in Zed's scoped key-value store
under the workspace root, as `history:<flow_id>`, capped at
`MAX_HISTORY_ENTRIES` per flow. An entry has a `stopped` flag, and entries
saved before it existed load as not stopped.

Related crates: [`workflow_engine`](../workflow_engine/README.md) (execution)
and [`designer_panel`](../designer_panel/README.md) (the canvas).

## Differences from Forge

- No favourites, tags or separate flows index table.
- Runs started here record final per-action outcomes only, and show Stop and
  the server links but not per-node status or the run log. For those, run the
  flow from the Designer.

## Development

```sh
cargo check -p flows_panel -j 8
cargo test -p flows_panel -j 8
```

The tests cover run-history persistence, and that the Designer's database
table has a single owner (`designer_panel`).

To run the tests of every fork crate at once: `script/test-fork-crates.ps1`.
