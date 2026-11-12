# helm_panel

"Helm" — a left-dock GitHub client for browsing and managing repositories,
organisations, issues, pull requests, releases and Actions runs.

## Why it exists

To handle everyday GitHub chores — find a repository, clone it, check an
Actions run, accept an invitation — without switching to the browser. Ported
from Forge's GitHub panel, with the UI rebuilt on `gpui_component` widgets in
place of the old `forge_ui` crate.

## Requirements

A GitHub account, and `git` on PATH for cloning. The GitHub CLI is not
needed: Helm signs in through its own OAuth App ("Helm for Forge") and keeps
each account's token in the system credential store (Windows Credential
Manager on Windows), under `helm://github.com/<login>`. Tokens are never
written to `settings.json`.

## Using it

Open with `ctrl-k h` (`cmd-k h` on macOS), the status bar icon (tooltip
"Helm"), or `helm panel: toggle focus`.

### First run

1. **Auth** — **Login with GitHub** shows a one-time code and a link; enter
   the code on GitHub and Helm signs in. **Use a token instead** takes a
   personal access token (a classic one needs `repo`, `read:org` and
   `user`; a fine-grained one limits Helm to the repositories you choose).
   **Use GitHub CLI** (shown only when `gh` is installed) copies one of the
   GitHub CLI's accounts' token once; Helm keeps its own copy and does not
   run `gh` again for it. An account `gh` does not have yet can be signed in
   to `gh` from there ("Account not listed?"): Helm runs
   `gh auth login --web`, shows its code, imports the account, and switches
   `gh` back to the account that was active before, so git keeps pushing as
   it did. Helm checks that the token has the `repo` scope
   and prompts for it if missing.
2. **Menu** — your profile summary and the main sections.

An organisation that restricts third-party apps does not show up, and its
private repositories cannot be seen, until its owners approve Helm. The
sign-in screen links to Helm's page in your GitHub settings, where you can
request that.

### Accounts

You can sign in to several GitHub accounts. The account button next to
"Helm" in the panel header switches between them without signing out, adds
an account, makes one the default, signs out of one (its token is deleted;
it stays listed) or removes it. Accounts imported from the GitHub CLI show
"via GitHub CLI"; a missing scope for one of them is added in `gh`
(`gh auth refresh --scopes <scope>`) and the token imported again, since
Helm's own sign-in would replace it with a token of Helm's app.

Each workspace remembers which account it uses; a workspace that never
chose one uses the default account.

Commits are authored with a name and email chosen per account (the
account menu's **Commit email…**, or the git panel's one-time question).
Helm writes them into each repository's own git config when a workspace
uses that account, and asks first if the repository already names someone
else. The global git config is never changed. If the global git email is
already one of the account's verified addresses, it is used without asking.

Pushing, pulling and fetching from the git panel log in to github.com as
the workspace's account: Helm hands git the account's token for those
commands (as an `Authorization` header in `GIT_CONFIG_*` environment
variables, through `ide_env`), so `gh auth switch` is not needed and two
windows can push as two accounts. Git in Zed's terminals does the same
through Zed's own credential helper (`crates/git_credential`), which is
asked first for github.com and answers from the workspace's account when
git asks; outside a Helm-known repository it answers nothing and your own
helpers (`gh`, Git Credential Manager) answer as before. Other hosts and
SSH remotes keep git's own login. When Helm knows the account
cannot do it, the git panel's Push / Pull / Fetch button is disabled and the
command is refused with the account that can.

When the workspace is a git repository on github.com, Helm checks whether
the workspace's account can push to it. If it cannot, Helm asks your other
signed-in accounts in the background (switching nothing), and when one of
them can push, a notification offers **Switch to …**. It is offered once
per account and repository in a session, and checked again when the
repository changes or GitHub rejects a change as not allowed. The list of accounts is
`helm_panel.accounts` in settings, and the default is
`helm_panel.default_account`; the tokens are only in the credential store,
so an account listed on a machine without its token shows "Sign in as …".

### What you can do

From the menu: **Repositories**, **Search GitHub**, **Organizations**,
**Invitations**, **Edit Profile**, **Account Security**, **Create Repository**
and **Logout**. Search GitHub searches repositories across GitHub and opens a
result in the existing repository detail view.

Inside a repository:

| Section | Shows |
| --- | --- |
| Branches | Branch list |
| Collaborators | Add or remove collaborators |
| Issues | Issue list and detail with comments, and a box to post a comment |
| Pull requests | List, detail, create, edit, close and reopen, merge, post a comment; the detail shows its checks and whether it can be merged |
| Releases | List and create |
| Packages | Packages; opening one shows its versions above the list |
| Commits | Recent commits |
| Actions | Workflow runs; a run opens as its own tab with jobs drawn as a graph |
| Deployments | Deployment list |
| Tags | Tag list |
| Traffic | Views, clones, referrers and popular paths |
| Security | Dependabot and secret-scanning alerts |

Repository settings and topics can be edited, and other users' profiles
viewed.

### Editing a repository

The settings icon on a repository ("Edit repository") opens a dialog for its name, description, homepage,
topics, and switches for Issues, Projects, Wiki and Discussions.

The **Private** switch changes the repository's visibility. It starts at the
repository's current setting, and a warning line appears under it as soon as
you flip it, saying what saving will do. Visibility is only sent to GitHub
when the switch was actually changed, so saving other edits never touches it.

Making a repository public exposes its code and history to everyone, and
GitHub may refuse the change depending on organisation policy or your plan.
A refusal is handled as described below.

### Pull requests

Opening a pull request fetches it on its own, because the list does not say
whether it merged or can be merged, and then loads the checks that ran on its
head commit.

- **Merge status.** GitHub's own verdict (`mergeable_state`) is shown as it is:
  ready, ready with a caution (checks that are not required are failing), or
  blocked (conflicts, a branch behind its base, a required review or check
  missing, a draft). While GitHub has not worked it out the line says so, and
  Helm asks again every two seconds, up to four times. **Refresh** asks again.
  Helm does not try to work out branch protection itself.
- **Checks.** The check runs of the head commit (GitHub Actions and other
  checks), each with its result and a link to its page. Legacy commit
  statuses are not shown.
- **Edit** changes the title and description. **Close** and **Reopen** change
  the state, and a merged pull request has neither.
- **Merge** is enabled only when GitHub says the pull request can be merged.
  A dialog names the pull request and its target branch and asks for the
  method (only those the repository allows, when it says), the commit title
  (not for a rebase) and whether to delete the head branch. The branch is only
  offered when it lives in the repository itself and is not its default
  branch. The merge is sent with the head commit the user saw, so GitHub
  refuses it if the branch moved in the meantime. A branch that cannot be
  deleted afterwards is not reported as a failure, because the merge itself
  succeeded.
- **Comments** on pull requests and issues are posted from the box under the
  thread. The box is emptied once GitHub has accepted the comment, so one that
  failed is still there to send again.

- **Staying current.** While an issue or pull request is open, Helm checks it
  for news every 30 seconds (every 15 while a pull request's checks are still
  running): new comments, the merge status and the checks. The check stops
  when you leave the item. It uses the same conditional requests as the rest
  of Helm, which GitHub answers with "unchanged" at no cost to the rate limit,
  and it is silent when it fails. **Refresh** does the same at once, and the
  comment box is never touched by either.

Marking a draft ready for review, reviews, and the changed files are not
done; use GitHub for those.

### Scopes Helm asks for

Signing in asks GitHub for `repo`, `read:org` and `user`:

- `repo`: your repositories, public and private, and changing them:
  creating and editing repositories, pull requests, releases,
  collaborators, and the git token for push, pull and fetch.
- `read:org`: the organisations you belong to (the Organizations screen
  and the Owner list when creating a repository).
- `user`: editing your GitHub profile from Zed, and reading your verified
  email addresses for the commit identity. `user:email` alone would cover
  the emails, but the first profile edit would then need a second sign-in
  for `user`, so Helm asks for it up front.

### When GitHub rejects a change

Every change Helm sends — creating or editing a repository, pull requests,
releases, collaborators, invitations, your profile — goes through one path.
If GitHub rejects one with a permission error, Helm checks the scopes GitHub
reported for the token to see whether it has the scope that action needs:

| Action | Scope |
| --- | --- |
| Repositories, pull requests, releases, collaborators, repo invitations | `repo` |
| Edit profile | `user` |
| Organisation invitations | `write:org` |

- **Scope missing:** Helm switches to the auth screen with an
  "Authorize <scope> scope" prompt. The button signs in again for the same
  account, asking for that scope on top of the ones it has, and shows the
  code and link. If the browser signs in a different account, Helm stores
  nothing and offers to try again or add that account instead. Once you
  have authorized, Helm returns you to the screen you were on and sends the
  change again. It retries once; a second rejection is reported as a normal
  failure.
- **Scope present, or not reported:** you get a "Failed to …"
  notification. Your account lacks rights on that resource, or the
  organisation blocks the change, and re-authorizing cannot fix that.
  Fine-grained tokens do not report scopes, so for them every rejection is
  reported this way.
- **No longer signed in:** you get the login prompt, and the change is sent
  again after you sign in.

### Cloning

**Clone** runs `git clone` as the account the window uses and streams its
output to a progress screen, then opens the clone as a new workspace. The
token is handed to `git` for that one command and is not stored in the
clone. **Cancel** stops the clone and removes the half-cloned folder.

Helm does not install dependencies itself: when the new workspace has a
`package.json` and no `node_modules`, it offers `npm install`. Pushing and
pulling in the clone later use git's own credential helper (Git Credential
Manager, or `gh` if you set that up), not Helm's token.

### Pages

Lists show ten rows at a time, with **Back** and **Next** arrows and "Page 2
of 7" under the list when there is more than one page.

- Issues, pull requests, branches, collaborators, releases, tags, commits,
  Actions runs and deployments ask GitHub for one page at a time.
- Repositories are fetched in full, because the search box filters them, and
  are then shown ten at a time. Typing in the search box goes back to the
  first page of what matches.
- On Issues a page can show fewer than ten rows while more pages follow.
  GitHub counts pull requests as issues in its pages; Helm leaves them out.
- Packages, security alerts, invitations and organisations are not paged
  and still show at most the first 100.

### Keyboard

Click a list to focus it, then:

| Key | Action |
| --- | --- |
| `up` / `down` | Move the selection |
| `enter` | Open the selected row, where rows open something |

A click on a row does the same as `enter`. On read-only lists (branches,
commits, deployments, tags, security alerts) neither does anything.

Invitations are accepted and declined only with a row's **Accept** and
decline buttons, never by `enter` or a click on the row, so one cannot be
accepted by accident.

## How it is wired

- `helm_panel::init(cx)` registers `helm_panel::ToggleFocus` and the key
  bindings for the profile menu (context `HelmRowList`). The list screens
  get theirs from `gpui_component`'s `List` (context `List`).
- `HelmPanel::load(workspace, cx)` is awaited in `initialize_panels`
  (`crates/zed/src/zed.rs`).
- Implements `workspace::dock::Panel`, docked left by default (the
  `helm_panel.dock` setting moves it left or right; `helm_panel.button` hides
  its status bar button), `activation_priority() = 21`.

### Actions

In the `helm_panel` namespace: `ToggleFocus`, `SelectNextRow`,
`SelectPrevRow`, `OpenSelectedRow`, `OpenRepositoryWorkspace`.

## Layout

Every screen's code is an `impl HelmPanel` block in its own file. The struct
itself, with all of its fields, stays in `src/helm_panel.rs`. See
`Helm_Refactor_Plan.md` for why it is laid out this way and what comes next.

| File | Contents |
| --- | --- |
| `src/helm_panel.rs` | `init`, the actions, the `HelmPanel` struct, `load`, and its `Panel` and `Render` impls |
| `src/section.rs` | `Section<T>`: one list's rows, load state and last error, and the loaders that fill one |
| `src/list_view.rs` | The shared list screen, on `gpui_component`'s `List` |
| `src/lists.rs` | Each list screen's widget: where its rows are, how a row is drawn, what `enter` does |
| `src/loading.rs` | The shared loader for screens that show one thing, not a list |
| `src/state.rs` | Which screen is showing, load state, menu rows |
| `src/navigation.rs` | Switching screens, back and forward, the navigation bar |
| `src/auth.rs` | Sign-in (device flow or pasted token), scope authorization, sign-out, and the auth screen |
| `src/accounts.rs` | Tokens in the credential store, the account list in settings, and the event that tells every panel when an account signs in or out |
| `src/account_switcher.rs` | The account menu, switching account, and each workspace's remembered account |
| `src/commit_identity.rs` | The commit name and email per account, written into the workspace repository's own git config |
| `src/repo_access.rs` | Whether the workspace's account can push to the workspace's GitHub repository, and the offer to switch to one that can |
| `src/gh_cli_import.rs` | "Use GitHub CLI": listing `gh`'s accounts, importing one's token, and its missing-scope prompt |
| `src/changes.rs` | `HelmAction` and `run_action`: the one path every change to GitHub goes through |
| `src/profile.rs` | Your profile and menu, the identity header, other users' profiles, edit profile |
| `src/orgs.rs` | Organisation list and detail |
| `src/repos.rs` | Repository list and detail, create and edit dialogs |
| `src/clone.rs` | The clone dialog, progress, and opening the result |
| `src/branches.rs` | Branches |
| `src/collaborators.rs` | Collaborators and their dialogs |
| `src/invitations.rs` | Repository and organisation invitations |
| `src/issues.rs` | Issues, plus the state filter and comment thread shared with pull requests |
| `src/pulls.rs` | Pull requests and creating one |
| `src/releases_packages.rs` | Releases, packages and tags |
| `src/activity.rs` | Commits, Actions runs and deployments |
| `src/insights.rs` | Traffic and security alerts |
| `src/repository_modal.rs` | The modal used for Helm's forms and confirmations |
| `src/workflow_run_tab.rs` | A workflow run as its own tab, with its jobs as a graph |
| `src/widgets.rs` | Small shared helpers |

The GitHub code itself (the device flow, the REST endpoints, cloning, the
response types) is in [`helm_backend`](../helm_backend/README.md).

### How requests work

The panel calls [`helm_backend`](../helm_backend/README.md) for everything
that touches GitHub. That crate runs on tokio, so UI code goes through
`helm_backend::on_tokio` rather than awaiting it directly on GPUI's
executor. In practice a screen never does that by hand: list screens load
through `load_section` (`section.rs`) and the others through `load_with`
(`loading.rs`).

A clone broadcasts each output line through a channel on `GhState`, which
the progress screen subscribes to.

Switching account gives the panel a new `GhState`. Every loader checks,
when its answer arrives, that the panel still has the state it started with
(`HelmView::is_current_state`), and drops the answer otherwise, so one
account's data never appears under another's.

To add a list screen: add the endpoint and its response type in
`helm_backend`, a `Section` field on the panel, a loader, a row function, an
entry in `lists.rs`, and a render function that calls `list_screen`.

## Networks, certificates and the credential helper

- **Proxy.** Helm's requests to GitHub go through Zed's HTTP client, which
  uses Zed's `"proxy"` setting, or the `HTTPS_PROXY` / `HTTP_PROXY`
  environment variables. On Windows the system proxy (Internet Options, or
  a PAC script) is not read. Behind a company proxy, set
  `"proxy": "http://proxy.example:8080"` in Zed's settings, and for git
  (clone, push, pull, fetch) `git config --global http.proxy
  http://proxy.example:8080`, since git does not read the system proxy
  either.
- **Certificates.** Requests are checked against the operating system's
  certificate store (on Windows, the one `certmgr` shows), so a company's
  inspection certificate installed there is trusted. Only if that store
  cannot be loaded does Zed fall back to the root certificates it ships
  with.
- **The credential helper.** Git in a Zed terminal asks Zed for the
  workspace account's token (`git_credential` crate) over a socket in a
  folder named `zed-git-credential*` under your temporary folder, which
  only your Windows account and administrators can open. A program running
  as you could ask it for a token, but it could equally read the token
  from Windows Credential Manager (`helm://github.com/<login>`), so the
  helper exposes nothing new. The token is never put in a terminal's
  environment, and only github.com over HTTPS is answered.
- **Several windows.** Each window's Helm panel gives git the settings for
  its own repository folder, and takes them back when the window closes.
  If the same folder is open in two windows with different accounts, git
  can only log in as one of them there: the window that set it up last
  wins, Helm says so in a toast, and closing that window hands the folder
  back to the other.

## Security at a glance

Short answers for anyone reviewing Helm before it is used at a company.

- **What can Helm access?** What the account can, within the scopes it
  asks for: `repo`, `read:org` and `user` (see "Scopes Helm asks for").
  Organisations that restrict OAuth Apps see nothing until an owner
  approves Helm, and organisations with single sign-on need the sign-in
  authorized for them.
- **Where is the token kept?** In the operating system's credential store
  (Windows Credential Manager, the macOS Keychain, the Secret Service on
  Linux), as `helm://github.com/<login>`. Never in `settings.json`, which
  only lists the account names, and never in Zed's log. A development
  build stores credentials in a plain-text file only when
  `ZED_DEVELOPMENT_USE_FILE_CREDENTIALS` is set, for throwaway testing; an
  older file of that kind is moved into the credential store at startup.
- **How long does a token last?** Helm's OAuth App tokens do not expire on
  their own. "Sign out of <name>" deletes Helm's copy on this machine but
  leaves the token valid on GitHub. "Sign out of <name> and revoke…" in
  the account menu also opens the GitHub page where it is revoked, which
  ends it on every machine: Helm's page under Authorized OAuth Apps, the
  token settings for a pasted token, or the GitHub CLI's entry for an
  imported one (revoking that signs `gh` out too). Helm cannot revoke it by
  itself: GitHub only allows that with the OAuth App's client secret, and
  an app installed on users' machines cannot keep one secret, so Helm has
  none.
- **Where does the token go?** Only to GitHub: to `api.github.com` for
  Helm's requests, and to `github.com` for git over HTTPS. For the git
  panel's push, pull and fetch it is passed to that one `git` process as
  an `Authorization` header in `GIT_CONFIG_*` environment variables, and
  never written to a git config file. Git in a Zed terminal asks Zed's
  credential helper for it instead, so it is never in a terminal's
  environment (see the section above). Other hosts never get it.
- **What about the GitHub CLI?** "Use GitHub CLI" copies one `gh` account's
  token once. Helm keeps its own copy from then on and does not run `gh`
  again; signing out of `gh` does not sign Helm out.
- **Who decides what it may do?** GitHub: an organisation's OAuth App
  policy and single sign-on, the account's own permissions on each
  repository, and the scopes above. Helm does not get around any of them;
  it tells you which one stopped a request, with a button to fix it on
  GitHub where there is one.

## Things to know

- Before tagging a release that changes accounts, git identity or
  credentials, run `REGRESSION_MATRIX.md` (account × repository × action:
  author, committer, and which account pushed), with
  `script/helm-identity-check.ps1` to read the results.

- `src/helm_panel.rs` is large (several thousand lines) and holds every
  screen. Search for the `HelmScreen` variant you need rather than reading
  top to bottom.
- This crate is licensed `Apache-2.0`, like the other fork panels. It links
  Zed's `workspace` and `ui` crates, which are `GPL-3.0-or-later`, so the
  built application as a whole is still distributed under the GPL.

## Development

```sh
cargo check -p helm_panel -j 8
cargo test -p helm_panel -j 8
```

The tests cover the decisions behind the re-authorization flow (which errors
count as permission errors, scope matching, which scope each action needs).
Nothing in them talks to GitHub.

To run the tests of every fork crate at once: `script/test-fork-crates.ps1`.
