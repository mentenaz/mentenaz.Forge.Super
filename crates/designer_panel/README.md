# designer_panel

The workflow Designer: a tab that shows a `.flow.json` file as a node graph
you can edit, save and run.

## Why it exists

A workflow is a dependency graph, and JSON is a poor way to read one. The
Designer draws each action as a node and each `runAfter` dependency as an
edge, lets you configure actions through generated forms instead of
hand-written JSON, and shows which action is running while a flow executes.

Ported from Forge's `designer_panel`, limited to the `.flow.json` format.

## Using it

### Opening a flow

Any `.flow.json` file opens in the Designer instead of the text editor —
double-click it in the project panel, use quick-open, or click **Graph** on a
row in the Flows panel. Open Designer tabs are restored with the workspace.

A file that fails to parse still opens a tab, with the error shown in a
banner inside it.

### The three panes

| Pane | Purpose |
| --- | --- |
| Action catalog | Searchable list of every action type, grouped by function |
| Canvas | The flow graph: pan, zoom, drag nodes, connect handles |
| Inspector | Properties of the selected node |

### Adding actions

Three ways, all drawing on the same registry:

- Click a catalog row (or press Enter on it) — the action is added as the
  next step after the selected action, or near the centre of the canvas when
  nothing is selected. With a loop, a branch, or an If/Try selected, it goes
  inside it, after the body's last action.
- Drag a catalog row onto the canvas to place it exactly. Dropping onto a
  loop or a branch puts the action inside it; dropping onto an If or Try
  puts it in the True or Try branch.
- Right-click the canvas and choose **Add Action**. On an action it adds the
  next step; on a container or branch it adds inside it.

To put an action between two that are already connected, hover the
connection: a **+** appears at its middle. Click it and pick an action. The
new action takes over the connection, and the one below it moves down, with
everything after it, to make room.

A connection means "run this after that succeeded". To change that, click
the connection: the inspector offers **Succeeded**, **Failed** (an error
path) and **Either**, and can delete it.

An action registered without an executor is listed but disabled. None are
right now: Send Email, DB Migration and AI Check are parked as future
features and do not appear in the catalog (see
[`workflow_engine`](../workflow_engine/README.md#future-features)).

### Toolbar

| Button | Does |
| --- | --- |
| Run | Run the flow. Disabled while blocking validation errors remain |
| Stop | Takes Run's place while a run is live: stops the run and the servers it started |
| Save | Write the flow and its layout |
| Fit View | Zoom to show the whole graph |
| Raw | Open the underlying JSON as text |
| Run log | Show or hide the run log under the canvas (appears once a run has produced something) |
| Results | Open the Run Results tab for this flow |
| Clear results | Clear run results and status colours from the canvas |
| Issues badge | Show what is blocking Run |

While a run is live the toolbar also names the node that is running, with a
spinner, and a short status ("Stopping…", "Run stopped").

A flow with unsaved edits shows the usual unsaved marker on its tab, saves
with the workspace save keybinding as well as the Save button, and asks
before closing. If the file changes on disk while there are unsaved edits,
the change is not loaded over them: a banner offers **Reload from disk**, and
saving keeps your edits instead.

Incomplete flows can be saved as drafts. Validation problems are shown on the
affected nodes and in the inspector, and only Run is blocked.

### Editing an action

Select a node to edit its properties in the inspector. **Delete Node**
removes it.

Required inputs always have a row. Optional ones are listed under **Add an
optional input** until you add them, and each has a remove button once
added. An optional input left blank is not written to the file. An input
with a fixed set of values, such as an HTTP method, is a dropdown.

Under an expression field, **Insert a reference** lists the outputs of the
actions that run before the selected one. After a run, **From the last run**
adds what those actions actually returned, to any depth: the fields inside an
HTTP response, the keys a script printed, and inside a Foreach the fields of
one `$item`. They are observations of one run, so a different response can
be shaped differently, and editing an action drops the samples for it and
everything after it until the next run.

Two input kinds get a real editor rather than a text field:

- **Scripts** — a script action runs a file.
  - Adding a Script creates one for it at
    `<flow>.flow-scripts/<action-id>.py`, from a template that already reads
    its input and prints a result. **Create a script file** does the same
    for Python, Node (`.mjs`), PowerShell or Bash; a file that is already
    there is reused, never overwritten.
  - **File Path** can point at any other file in the project instead. The
    runtime comes from the extension, or from the **Runtime** override.
  - **Open Script** opens the file in an editor tab, with full
    language-server support.
  - A script reads the run through a small helper in the same folder, and
    ends by handing back its result:

    ```js
    import { variables, item, args, actions, output } from "./forge.mjs";
    output({ count: variables.ids.length });
    ```

    | Language | Helper | Use |
    | --- | --- | --- |
    | Node (modules) | `forge.mjs` | `import { variables } from "./forge.mjs"` |
    | Node (`require`) | `forge.cjs` | `const { variables } = require("./forge.cjs")` |
    | Python | `forge.py` | `from forge import variables` |
    | PowerShell | `forge.ps1` | `. "$PSScriptRoot/forge.ps1"`, then `$Variables` |

    `variables` is the flow's variables, `item` the current Foreach item,
    `args` this action's `args` (or `argsExpression` result), and `actions`
    the outputs of the actions that have finished, by id. The helpers and
    the type declarations beside them (`forge.d.mts`, `forge.d.cts`) are
    generated: they are written when a script is created and refreshed on
    save and run, and the types follow the flow's variables and its last
    run. Don't edit them. Bash has no helper.
  - Without the helper, `args` also still arrives as JSON on standard input,
    and the JSON object a script prints last becomes its outputs.
  - Inline code is no longer created or edited. A flow that still has some
    is converted when it is opened: each inline script's code is written to
    its file and the action refers to that file from then on. The flow file
    itself changes on the next save.
- **HTTP bodies** — **Edit Body** opens the request body as a JSON file at
  `<flow>.flow-http/<action-id>.body.json`, with JSON validation. A request
  with no body yet gets an empty `{}` one.

A request body is synchronised with the flow in both directions. Script
files and body files are ordinary files you can commit, and a flow needs its
`.flow-scripts` folder alongside it to run.

### Running

While a flow runs, nodes are coloured by status and a log pane shows
progress. Selecting a node shows its result in the inspector.

A node is **Running**, **Ready**, **Succeeded**, **Failed** or **Skipped**.
**Ready** belongs to a node that starts a server (`StartProcess` with a
`ready_*` setting): the server is up and still running, so the node is not
finished, but the nodes after it can start. How a command node behaves is
described in [`workflow_engine`](../workflow_engine/README.md#commands-and-servers-startprocess-and-waitforport):
a command that ends (a build) finishes when it exits, and a server stays up
until the run is stopped.

#### Servers and Stop

A run lasts as long as the servers it started, so the toolbar keeps showing
**Stop** after the last node has finished. Stop ends the run, kills every
server it started and skips the nodes that had not started. A stopped run is
not a failure. A server that exits with an error fails the run and stops the
others, and so does a node that fails.

- **Re-running** a flow stops the servers that an earlier run of the same
  flow left on the same node, so there is one server per port.
- **Closing the tab** stops the run it started. **Quitting the app** stops
  every server a flow started; on Windows they are also tied to the app, so
  they end even if it crashes.
- A `StartProcess` with no `ready_*` setting waits for its command to exit,
  so a dev server there holds back every node after it. The inspector warns
  about a command that looks like a server.

#### The run log

The log under the canvas lists each node's status changes and, in monospace
under its node, the lines its command prints, as they arrive. Terminal colour
codes are removed, empty lines are left out, and error output is red. Only
the newest 2,000 output lines are kept; status lines are never dropped.
Selecting a row selects its node.

#### The servers bar

Once a server is Ready and has an address, a bar under the toolbar lists it:
`Start frontend: http://127.0.0.1:5173/` with **Open** (browser) and **Copy**
buttons. The address is the node's `url` input, else the first local address
the server printed, else its `ready_url`, else `localhost:` and its
`ready_port` (copy only, as a port alone is not known to be a web page). A
server with nothing to show is left out, and the bar is gone once the run
ends.

A Foreach body runs once per item, so an action inside one has a result per
iteration. The inspector shows a pager for the loop, `For each · <loop>
‹ 2 / 12 ›`, with the item that iteration ran for, and the result shown
below is that iteration's. Stepping the pager changes the iteration for
every action in the loop's body. On the canvas, a body action's colour is
its most recent iteration's, and the log marks each line with its iteration
(`Charge [2/12]`). **Results** opens a read-only Run Results tab for the most recent
run. It keeps updating even if the Designer tab that started the run is
closed.

## Walkthrough: start a backend and a frontend

A flow that brings up two dev servers and gives you their links.

1. In the Flows panel choose **New Workflow** (or **Add Task Chain** to have
   the services detected for you). The flow opens in the Designer.
2. Click **Start Process** in the catalog twice and label the nodes, for
   example *Start backend* and *Start frontend*.
3. Select *Start backend* and fill in the inspector:
   - `command`: `python` (or the full path to the project's `.venv`
     interpreter, since the project's virtual environment is not activated)
   - `args`: `["manage.py", "runserver"]`
   - `cwd`: `backend`
   - `url`: `http://localhost:8000`
   - **Add an optional input → `ready_port`**: `8000`
4. Do the same for *Start frontend*: `command` `npm`, `args`
   `["run", "dev", "--", "--host", "127.0.0.1", "--port", "5173",
   "--strictPort"]`, `cwd` `frontend`, `url` `http://localhost:5173`,
   `ready_port` `5173`.
5. Without `ready_port` (or `ready_url` or `ready_text`) the node never turns
   Ready, because a server never exits. This is the usual reason a run seems
   stuck on "Running…".
6. Press **Run**. Both nodes go Running, then Ready, and the servers bar
   shows both links with **Open** and **Copy**. The run log shows what each
   server prints.
7. Press **Stop** when you are done: both servers are killed. Closing the tab
   does the same.

To run something after a server is up (a health check, a seed script), add
the node after it; it starts as soon as the server is Ready. A `Wait For
Port` node still works for a server you did not start from the flow, or to
wait for an HTTP address to answer.

## Files the Designer writes

| Path | Contents |
| --- | --- |
| `<name>.flow.json` | The workflow itself |
| `<name>.flow.layout.json` | Node positions |
| `<name>.flow-scripts/` | Script files |
| `<name>.flow-http/` | HTTP body companions |

Node positions never go into `.flow.json`, so moving a node does not show up
as a change to the flow's behaviour. A flow with no layout file is laid out
automatically in layers.

## How it is wired

- `designer_panel::init(cx)` (from `crates/zed/src/main.rs`) registers the
  workspace-wide action handlers and calls
  `workspace::register_project_item::<DesignerPanel>` and
  `register_serializable_item::<DesignerPanel>`. It also registers the quit
  hook that calls `workflow_engine::stop_all_processes()` (gpui gives quit
  handlers about 200 ms, so on Windows the engine's job objects do the real
  work).
- `FlowFile` (`src/project_item.rs`) is the minimal `project::ProjectItem`
  that claims paths ending in `.flow.json`. Project item types are tried
  most-recently-registered first, and `try_open` returns `None` for anything
  else, so other file types are unaffected.
- `DesignerPanel::open(path, root, workspace, project, window, cx)` is the
  programmatic entry point, used by the Flows panel.
- `DesignerPanel` is a `workspace::Item` — one tab per flow, not a dock
  panel.

### Actions

In the `designer_panel` namespace, each carrying a path: `ViewRaw`,
`ViewScriptCompanion`, `ViewHttpBody`, `ViewRunResults`, plus
`ShowInvalidFlowToast`. They are dispatched to workspace-wide handlers
because a Designer opened through the project-item path has no workspace
handle of its own. None are bindable from a keymap (`no_json`).

`ViewRaw` builds a plain editor for the buffer directly. Going through
`open_abs_path` would route back to the Designer.

## Layout

| File | Contents |
| --- | --- |
| `src/designer_panel.rs` | The tab: canvas, toolbar, inspector, run handling |
| `src/catalog.rs` | Action catalog (a `gpui_component` list over the registry) |
| `src/workflow_json.rs` | Load/save between `flow.json` and `gpui_flow::FlowState`; auto-layout |
| `src/project_item.rs` | `FlowFile`, which routes `.flow.json` here |
| `src/script_companion.rs` | Inline script companion files |
| `src/http_companion.rs` | HTTP body companion files |
| `src/run_state.rs` | Thread-safe run state shared with Run Results: node phases (including `Ready`), the transcript, and the lines commands print |
| `src/run_results.rs` | The Run Results tab |
| `src/persistence.rs` | Restoring open Designer tabs |

Depends on [`workflow_engine`](../workflow_engine/README.md) for the schema,
registry and execution, and on [`gpui_flow`](../gpui_flow/README.md) for the
canvas.

### Implementation notes

- **If and Try have two bodies** (then/else, try/catch) that are just fields
  on one action in JSON, but need two container nodes on the canvas.
  `workflow_json::load` synthesises wrapper nodes with reserved types
  (`__branch_then`, `__branch_else`, `__branch_try`, `__branch_catch`) and
  `save` unwraps them again. They are never written to the file. Both
  wrappers always exist, so there is somewhere to drop an action into a
  branch the file doesn't have yet; an empty one is only written back if the
  file already had it, because an empty `catch` changes what a `Try` does.
- **Deleting a container deletes its body.** The canvas's Delete key removes
  only the selected node, so the Designer removes whatever that leaves
  without a parent. Undo restores all of it.
- **Run status crosses threads.** The engine's status callback fires off the
  UI thread, so it fills an `Arc<Mutex<RunState>>` that the Designer and Run
  Results tabs poll about ten times a second. A command's output comes
  through the sink's separate output callback into the same state
  (`RunState::record_output`, capped at `MAX_OUTPUT_LINES`).
- **The panel keeps the run's `RunControl`.** The run is started with
  `run_workflow_controlled`; Stop calls `RunControl::stop`, the servers bar
  reads `RunControl::service_links`, and `Drop for DesignerPanel` stops the
  run when the tab closes. A stopped run is recorded as `stopped`, not failed.
- **HTTP bodies are compared semantically**, not byte for byte, so
  reformatting the JSON does not register as a conflict. A body that is not
  valid JSON yet is stored verbatim.

## Further reading

[`DESIGNER_PANEL_TODO.md`](DESIGNER_PANEL_TODO.md) tracks the agreed UX and
script-editing work, phase by phase.

## Development

```sh
cargo check -p designer_panel -j 8
cargo test -p designer_panel -j 8
```

To run the tests of every fork crate at once: `script/test-fork-crates.ps1`.
