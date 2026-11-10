# Forge Task Runner and Flow Engine: Findings

2026-11-09. The status section was added after the engine changes described in it.

## Summary

Forge's Script Runner and flow engine start commands with the system Python and never clean up after themselves, which produced a failed run, a misleading "Succeeded" status, and duplicate servers piling up on ports 8000 and 5173.

- **Wrong interpreter:** the Script Runner and the `StartProcess` node run `python` from the system PATH, not the project's `.venv`, so Django is not found.
- **Misleading status:** `StartProcess` reports Succeeded as soon as the process is spawned, even if it exits straight away.
- **Fixed port and address:** `WaitForPort` polls one port, so Vite moving to another port, or listening on IPv6 only, makes it time out.
- **No cleanup:** every run starts new servers and leaves the old ones, so three Django and two Vite servers were listening at once.

## Context

The findings come from running a throwaway Django + Vite React/TypeScript sandbox from inside Forge on Windows.

- **Layout:** `H:\sandbox\fullstack-play` with a `.venv`, a Django project in `backend`, and a Vite app in `frontend`.
- **Backend:** `python manage.py runserver`, with a `/api/health/` endpoint that the frontend calls.
- **Frontend:** `npm run dev`, expected on port 5173.
- **Forge tools involved:** the Script Runner panel, the workflow engine (flow `flow-20261109-091300` with `StartProcess` and `WaitForPort` nodes), and the Node and Processes panel.
- **Goal:** start both servers from one flow and confirm each is ready before continuing.

## Finding 1: commands run with the system Python, not the project venv

The Script Runner ran `python manage.py runserver 0.0.0.0:8000` in `H:\sandbox\fullstack-play\backend` and failed with `ModuleNotFoundError: No module named 'django'`.

Django is installed only in the `.venv` created for the sandbox. A plain command runner does not activate a virtual environment, so `python` resolved to the system interpreter. The same command worked in a terminal where the venv was active.

The flow engine has the same gap: the `StartProcess` node was configured with `command: python`, which resolves the same way.

## Finding 2: StartProcess reports Succeeded even when the process dies

In the first flow run, `Start backend` showed Succeeded after 42 ms and returned a PID, while `Wait For backend` failed after 30 seconds with `port 8000 not ready`.

The likely sequence: Forge spawned `python`, the process exited at once because Django was missing (Finding 1), and nothing ever listened on port 8000. `StartProcess` only confirms the spawn, so the real error was hidden and the user saw a timeout instead.

This cause is inferred from the matching Script Runner error. The flow run itself did not show the process output.

## Finding 3: WaitForPort checks one fixed port and address

After the backend was fixed, `Wait For backend` passed (`ready: true`) but `Wait For frontend` timed out on port 5173 while the Vite process had started successfully.

Two causes fit the evidence, and both come from `WaitForPort` watching a single port:

- **Port drift:** if 5173 is already taken, Vite quietly moves to the next free port. The server is alive, but nothing answers on 5173.
- **IPv4 vs IPv6:** `netstat` later showed Vite listening on both `127.0.0.1:5173` and `[::1]:5173`, from two different processes. A check against only one address can miss a server bound to the other. Django listened on IPv4, which fits the backend passing.

Which of the two caused the timeout was not confirmed, because the Vite process output was not captured.

## Finding 4: flow runs leak processes

Each flow run starts new servers and nothing stops the ones from earlier runs. After several runs, three listeners were on port 8000 and two on port 5173, and the Processes panel listed duplicate `vite` and `npm run dev` entries.

This matters because a request can reach an old server still running code from before the latest changes. That is a common cause of "I changed it and nothing happened". Windows appears to allow a second Django server to bind a port that is already in use, so the duplicates did not fail loudly (inferred, not tested).

The Django servers were not among the highlighted entries in the Processes panel (they may sit under its collapsed "Other" group), so they were confirmed with `netstat` and `Get-Process`.

## Evidence

Listening sockets found with `netstat -ano` after several flow runs:

| Port | Address | PID | Note |
| --- | --- | --- | --- |
| 8000 | 127.0.0.1 | 39792 | Django server |
| 8000 | 127.0.0.1 | 43664 | Django server |
| 8000 | 127.0.0.1 | 38228 | Django server |
| 5173 | 127.0.0.1 | 42632 | Vite (IPv4) |
| 5173 | [::1] | 31580 | Vite (IPv6) |

Other observations:

- Six small `python` processes (PIDs 18696, 18860, 37908, 38876, 40988, 43184) were running. None of the three port 8000 listener PIDs appears in that list, so they could not be matched to the Django servers.
- First flow run: `Start backend` Succeeded in 42 ms, then `Wait For backend` failed after about 32 seconds with `port 8000 not ready after 30000ms`.
- Second flow run: `Wait For backend` passed with `ready: true`, `Wait For frontend` failed after about 32 seconds on port 5173.
- Final run: all four nodes Succeeded, and the Processes panel listed two `vite` and two `npm run dev` entries.

## Workarounds

These work today without changing Forge. The final flow run succeeded, but which of these changes were applied to it was not confirmed.

**1. Point the backend at the venv's Python** in the `startbackend` node, using an absolute path:

```json
"inputs": {
  "command": "H:\\sandbox\\fullstack-play\\.venv\\Scripts\\python.exe",
  "args": ["manage.py", "runserver", "--noreload"],
  "cwd": "backend",
  "url": "http://localhost:8000"
}
```

`--noreload` avoids the auto-reloader's child process being orphaned when the parent is stopped. The cost is no automatic reload on code changes.

**2. Make the frontend port deterministic** in the `startfrontend` node:

```json
"args": ["run", "dev", "--", "--host", "127.0.0.1", "--port", "5173", "--strictPort"]
```

`--strictPort` makes Vite fail instead of moving to another port, and `--host 127.0.0.1` binds it to the address `WaitForPort` checks. Open the app at `http://127.0.0.1:5173` afterwards.

**3. Clear leftover servers** before a run:

```powershell
Get-NetTCPConnection -LocalPort 8000,5173 -State Listen -ErrorAction SilentlyContinue |
  Select-Object -ExpandProperty OwningProcess -Unique |
  ForEach-Object { Stop-Process -Id $_ -Force }
```

This could run as a `Script` node at the start of the flow.

## Recommended Forge improvements

Ordered by how much friction each removes:

1. **Track and stop what StartProcess starts.** Remember the PIDs a flow launched, and stop them when the flow is re-run or the project closes, or offer a "stop existing instance first" option. This removes the duplicate servers (Finding 4). The Processes panel already tracks PIDs.
2. **Surface process output and early exits.** Show stderr in the run log, and fail the node if the process exits within a short window. A missing module would then show as an immediate error instead of a 30-second timeout (Finding 2).
3. **Use the project's interpreter.** Let `StartProcess` and the Script Runner follow the selected toolchain, or offer a "use project Python" option that resolves `.venv\Scripts\python.exe` (Finding 1).
4. **Make readiness checks tolerant.** Let `WaitForPort` try both `127.0.0.1` and `[::1]`, and accept an HTTP URL such as `/api/health/` so it proves the app is serving and not only that a port is open (Finding 3).
5. **Detect port drift.** Warn when a dev server prints a different port from the one the flow is waiting on.
6. **Optional detached mode.** Allow a process to keep running after the flow ends, for cases where the servers should outlive the run.

## Status: what the engine now does

The branch `workflow-engine-process-lifecycle` (not yet merged to `main`) answers improvements 1, 2, 4 and part of 6. Each point says how far it has been checked.

| Improvement | State |
| --- | --- |
| 1. Track and stop what StartProcess starts | Done. Re-running a flow stops the servers an earlier run of the same flow left on the same node. A Stop button ends a live run and its servers. A failed node stops the run's servers. Closing the Designer tab or the Flows panel stops the run. Quitting calls `stop_all_processes`, and on Windows each process is also in a kill-on-close job object. |
| 2. Surface output and early exits | Done. A command's output reaches the run log as it prints, with terminal colour codes removed. A process that exits is reported with its exit code and its last lines, so a missing module is an immediate error, not a timeout. |
| 3. Use the project's interpreter | Not done. Workaround 1 still applies. |
| 4. Tolerant readiness checks | Done. `WaitForPort` tries `127.0.0.1` and `[::1]` and takes an optional `url`. `StartProcess` has its own `ready_port`, `ready_url`, `ready_text` and `ready_timeout_ms`. |
| 5. Detect port drift | Not done. Vite's printed address is now read to show a link, but nothing compares it with the port the flow waits on. |
| 6. Detached mode | Not done. A run now lasts as long as the servers it started, which is the opposite default. |

How a command node behaves now: a command that ends (a build) finishes its node when it exits, and a server (`npm run dev`) is `Ready` once its `ready_*` settings pass, then keeps running. Nodes after it start at that point. A server with no `ready_*` setting holds back everything after it, and the validator warns about a command that looks like one.

Also added: a Services block above the Designer's run log, and under a flow's row in the Flows panel, listing each running server with an address and open and copy buttons. The address is the node's `url`, else the first local address the server printed, else `ready_url`, else `localhost:` and the `ready_port`.

Checked by hand in the app: Stop kills both servers, and closing the tab stops them. Not yet checked in the app: quitting the whole app mid-run and finding no leftover processes; running a flow twice and finding one server per port; the Services block (the last commit was made before it had been compiled); and the Ready colour in the Designer. The engine's own tests pass, apart from one test that failed intermittently under load before it was hardened.

## Open questions

- [x] Which address does `WaitForPort` actually poll? It now tries `127.0.0.1` and then `[::1]`, and the node reports the one that answered as `address`.
- [ ] Did the frontend timeout come from Vite moving to another port, or from the IPv4/IPv6 mismatch? The Vite output from that run was not captured, and the new output capture would show it on a repeat.
- [x] Does Forge stop child processes when a flow ends, or only fail to stop them on re-run? It now stops them on re-run, on Stop, when a node fails, on tab close and on quit.
- [ ] Do the six `python` processes include the Django servers or reloaders, and are any of them orphans?
- [ ] Do the Django servers appear under the Processes panel's "Other" group?
- [ ] Does the Script Runner follow the selected toolchain when one is set, or never?
