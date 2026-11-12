//! What Helm asks GitHub for, as data.
//!
//! Each function here builds one [`ApiRequest`]: a method, a path and an
//! optional JSON body. Nothing is sent. `api.rs` sends them, and the tests
//! below check them, which is how the endpoints are covered without a
//! network.
//!
//! Every value placed in a path or a query string goes through [`seg`]
//! first, so a name containing `/`, `?` or `#` cannot change which endpoint
//! is called. That matters for more than safety: a container package is
//! commonly named `owner/image`, and its `/` has to be sent as `%2F`.

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiResponseFormat {
    Json,
    Raw,
    Diff,
    TextMatch,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiRequest {
    pub method: &'static str,
    /// Path and query, starting with `/`. The base URL is added on sending.
    pub path: String,
    pub body: Option<Value>,
    pub accept: ApiResponseFormat,
    /// GitHub maintains independent budgets for core, search and code search.
    pub rate_limit_resource: &'static str,
}

impl ApiRequest {
    /// The same request for page `page` (counting from 1) at `per_page`
    /// items a page, replacing any paging the request already asked for.
    pub fn page(mut self, page: u32, per_page: u32) -> ApiRequest {
        let (path, query) = match self.path.split_once('?') {
            Some((path, query)) => (path.to_string(), query.to_string()),
            None => (self.path.clone(), String::new()),
        };
        let mut params: Vec<String> = query
            .split('&')
            .filter(|param| !param.is_empty())
            .filter(|param| !param.starts_with("per_page=") && !param.starts_with("page="))
            .map(str::to_string)
            .collect();
        params.push(format!("per_page={per_page}"));
        params.push(format!("page={}", page.max(1)));
        self.path = format!("{path}?{}", params.join("&"));
        self
    }

    pub fn with_accept(mut self, accept: ApiResponseFormat) -> ApiRequest {
        self.accept = accept;
        self
    }
}

fn get(path: String) -> ApiRequest {
    ApiRequest {
        method: "GET",
        path,
        body: None,
        accept: ApiResponseFormat::Json,
        rate_limit_resource: "core",
    }
}

fn delete(path: String) -> ApiRequest {
    ApiRequest {
        method: "DELETE",
        path,
        body: None,
        accept: ApiResponseFormat::Json,
        rate_limit_resource: "core",
    }
}

fn with_body(method: &'static str, path: String, body: Value) -> ApiRequest {
    ApiRequest {
        method,
        path,
        body: Some(body),
        accept: ApiResponseFormat::Json,
        rate_limit_resource: "core",
    }
}

/// One value, made safe to place in a path or a query string.
fn seg(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// `/repos/{owner}/{name}` followed by `rest`.
fn repo_path(owner: &str, name: &str, rest: &str) -> String {
    format!("/repos/{}/{}{rest}", seg(owner), seg(name))
}

// ── Account ────────────────────────────────────────────────────────────

pub fn current_user() -> ApiRequest {
    get("/user".to_string())
}

/// The signed-in user's email addresses. Needs the `user` (or `user:email`)
/// scope.
pub fn current_user_emails() -> ApiRequest {
    get("/user/emails?per_page=100".to_string())
}

pub fn update_user(changes: Value) -> ApiRequest {
    with_body("PATCH", "/user".to_string(), changes)
}

pub fn user(username: &str) -> ApiRequest {
    get(format!("/users/{}", seg(username)))
}

/// The signed-in user's organisation memberships, active and pending.
pub fn org_memberships() -> ApiRequest {
    get("/user/memberships/orgs?per_page=100".to_string())
}

pub fn org_detail(org: &str) -> ApiRequest {
    get(format!("/orgs/{}", seg(org)))
}

/// An organisation's members. Someone outside the organisation is given its
/// public members only.
pub fn org_members(org: &str) -> ApiRequest {
    get(format!("/orgs/{}/members?per_page=100", seg(org)))
}

// ── Repositories ───────────────────────────────────────────────────────

/// The repositories of `owner`. `"self"` means the signed-in user's own.
pub fn repos(owner: &str) -> ApiRequest {
    if owner == "self" {
        get("/user/repos?per_page=100&sort=updated&affiliation=owner".to_string())
    } else {
        get(format!(
            "/orgs/{}/repos?per_page=100&sort=updated",
            seg(owner)
        ))
    }
}

pub fn repo(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, ""))
}

/// Creates a repository from `opts`. An `owner` in `opts` names the
/// organisation to create it in, and is not sent as part of the body;
/// without one the repository is the signed-in user's.
pub fn create_repo(opts: &Value) -> ApiRequest {
    match opts.get("owner").and_then(|owner| owner.as_str()) {
        Some(org) => {
            let mut body = opts.clone();
            if let Some(fields) = body.as_object_mut() {
                fields.remove("owner");
            }
            with_body("POST", format!("/orgs/{}/repos", seg(org)), body)
        }
        None => with_body("POST", "/user/repos".to_string(), opts.clone()),
    }
}

pub fn update_repo(owner: &str, name: &str, changes: Value) -> ApiRequest {
    with_body("PATCH", repo_path(owner, name, ""), changes)
}

pub fn update_topics(owner: &str, name: &str, topics: &[String]) -> ApiRequest {
    with_body(
        "PUT",
        repo_path(owner, name, "/topics"),
        json!({ "names": topics }),
    )
}

pub fn branches(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/branches?per_page=100"))
}

/// GitHub's GraphQL query for the branches and tags of a repository whose
/// names contain `$query`, anywhere in them. The REST API can only match
/// the start of a ref's name (`git/matching-refs`).
const SEARCH_REFS_QUERY: &str = "\
query($owner: String!, $name: String!, $query: String!, $first: Int!) {
  repository(owner: $owner, name: $name) {
    branches: refs(refPrefix: \"refs/heads/\", query: $query, first: $first, \
orderBy: {field: ALPHABETICAL, direction: ASC}) { nodes { name } }
    tags: refs(refPrefix: \"refs/tags/\", query: $query, first: $first, \
orderBy: {field: TAG_COMMIT_DATE, direction: DESC}) { nodes { name } }
  }
}";

/// Up to `first` branches and up to `first` tags of `owner/name` whose
/// names contain `query`, through GitHub's GraphQL API. GraphQL has an
/// allowance of its own, counted in points, so it is tracked apart from
/// the REST one.
pub fn search_refs(owner: &str, name: &str, query: &str, first: u32) -> ApiRequest {
    ApiRequest {
        rate_limit_resource: "graphql",
        ..with_body(
            "POST",
            "/graphql".to_string(),
            json!({
                "query": SEARCH_REFS_QUERY,
                "variables": {
                    "owner": owner,
                    "name": name,
                    "query": query,
                    "first": first,
                },
            }),
        )
    }
}

pub fn tags(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/tags?per_page=100"))
}

pub fn recent_commits(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/commits?per_page=100"))
}

pub fn recent_commits_for_ref(owner: &str, name: &str, ref_name: &str) -> ApiRequest {
    get(format!(
        "{}?sha={}",
        repo_path(owner, name, "/commits"),
        seg(ref_name)
    ))
}

/// Commits touching a file or folder at a ref.
pub fn commits_for_path(owner: &str, name: &str, path: &str, ref_name: &str) -> ApiRequest {
    get(format!(
        "{}?path={}&sha={}",
        repo_path(owner, name, "/commits"),
        seg(path),
        seg(ref_name)
    ))
}

/// Resolve a branch, tag, or commit name to its commit object.
pub fn commit(owner: &str, name: &str, ref_name: &str) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!("/commits/{}", seg(ref_name)),
    ))
}

/// A commit including its file summaries and patches.
pub fn commit_detail(owner: &str, name: &str, sha: &str) -> ApiRequest {
    commit(owner, name, sha)
}

/// A whole tree by its immutable tree SHA. When GitHub says `truncated`,
/// callers must load each child tree separately rather than trust the result.
pub fn tree(owner: &str, name: &str, tree_sha: &str, recursive: bool) -> ApiRequest {
    get(format!(
        "/repos/{}/{}/git/trees/{}?recursive={}",
        seg(owner),
        seg(name),
        seg(tree_sha),
        recursive
    ))
}

/// One non-recursive directory tree addressed by the tree SHA from its parent.
pub fn tree_dir(owner: &str, name: &str, tree_sha: &str) -> ApiRequest {
    tree(owner, name, tree_sha, false)
}

/// A Git tree by its immutable object SHA, used to expand a truncated tree.
pub fn tree_sha(owner: &str, name: &str, sha: &str) -> ApiRequest {
    tree(owner, name, sha, false)
}

/// A raw blob by its SHA, as a bytes payload. The caller decides whether it is
/// text, binary or LFS pointer.
pub fn blob(owner: &str, name: &str, sha: &str) -> ApiRequest {
    get(format!(
        "/repos/{}/{}/git/blobs/{}",
        seg(owner),
        seg(name),
        seg(sha)
    ))
    .with_accept(ApiResponseFormat::Raw)
}

/// The README of a repository or a folder at a ref.
pub fn readme(owner: &str, name: &str, ref_name: &str) -> ApiRequest {
    get(format!(
        "/repos/{}/{}/readme?ref={}",
        seg(owner),
        seg(name),
        seg(ref_name)
    ))
}

/// The README in a folder at a ref.
pub fn readme_at_path(owner: &str, name: &str, path: &str, ref_name: &str) -> ApiRequest {
    get(format!(
        "/repos/{}/{}/readme/{}?ref={}",
        seg(owner),
        seg(name),
        seg(path),
        seg(ref_name)
    ))
}

/// A two-ref comparison, showing which commits and files differ.
pub fn compare(owner: &str, name: &str, base: &str, head: &str) -> ApiRequest {
    get(format!(
        "/repos/{}/{}/compare/{}...{}",
        seg(owner),
        seg(name),
        seg(base),
        seg(head)
    ))
}

/// Search the whole GitHub repository index for a phrase.
pub fn search_repositories(query: &str, page: u32, per_page: u32) -> ApiRequest {
    let mut r = get(format!(
        "/search/repositories?q={}&per_page={}&page={}",
        seg(query),
        per_page,
        page.max(1)
    ));
    r.path = format!(
        "/search/repositories?q={}&per_page={}&page={}",
        seg(query),
        per_page,
        page.max(1)
    );
    r.rate_limit_resource = "search";
    r
}

/// Search the contents of GitHub code, returning match positions. The caller
/// decides which repo or default branch is in scope.
pub fn search_code(query: &str, repo: Option<&str>, page: u32, per_page: u32) -> ApiRequest {
    let q = match repo {
        Some(repo) => format!("repo:{} {}", repo, query),
        None => query.to_string(),
    };
    let mut req = get(format!(
        "/search/code?q={}&per_page={}&page={}",
        urlencoding::encode(&q),
        per_page,
        page.max(1)
    ));
    req.accept = ApiResponseFormat::TextMatch;
    req.rate_limit_resource = "code_search";
    req
}

// ── Collaboration ──────────────────────────────────────────────────────

pub fn issues(owner: &str, name: &str, state: &str) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!(
            "/issues?state={}&per_page=100&sort=updated&direction=desc",
            seg(state)
        ),
    ))
}

pub fn pulls(owner: &str, name: &str, state: &str) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!(
            "/pulls?state={}&per_page=100&sort=updated&direction=desc",
            seg(state)
        ),
    ))
}

pub fn pull_for_head(owner: &str, name: &str, head: &str) -> ApiRequest {
    get(format!(
        "{}?state=all&head={}&per_page=100&sort=updated&direction=desc",
        repo_path(owner, name, "/pulls"),
        seg(head)
    ))
}

pub fn combined_status(owner: &str, name: &str, ref_name: &str) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!("/commits/{}/status", seg(ref_name)),
    ))
}

/// The conversation on an issue or a pull request; GitHub serves both from
/// the issues endpoint.
pub fn issue_comments(owner: &str, name: &str, number: u64) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!("/issues/{number}/comments?per_page=100"),
    ))
}

pub fn create_pull(
    owner: &str,
    name: &str,
    head: &str,
    base: &str,
    title: &str,
    body: Option<&str>,
) -> ApiRequest {
    with_body(
        "POST",
        repo_path(owner, name, "/pulls"),
        json!({ "title": title, "head": head, "base": base, "body": body }),
    )
}

/// One pull request on its own, which (unlike the list) says whether it can
/// be merged.
pub fn pull(owner: &str, name: &str, number: u64) -> ApiRequest {
    get(repo_path(owner, name, &format!("/pulls/{number}")))
}

/// Closes, reopens or edits a pull request: `changes` holds any of `title`,
/// `body`, `state` (`open` or `closed`) and `base`.
pub fn update_pull(owner: &str, name: &str, number: u64, changes: Value) -> ApiRequest {
    with_body(
        "PATCH",
        repo_path(owner, name, &format!("/pulls/{number}")),
        changes,
    )
}

/// Posts a comment on an issue or a pull request's conversation.
pub fn create_issue_comment(owner: &str, name: &str, number: u64, body: &str) -> ApiRequest {
    with_body(
        "POST",
        repo_path(owner, name, &format!("/issues/{number}/comments")),
        json!({ "body": body }),
    )
}

/// The checks that ran on a commit. A single page of up to 100 is enough
/// for the pull request screen.
pub fn check_runs(owner: &str, name: &str, ref_name: &str) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!("/commits/{}/check-runs?per_page=100", seg(ref_name)),
    ))
}

/// Merges a pull request. `method` is `merge`, `squash` or `rebase`.
/// `sha` is the head the user saw: GitHub refuses the merge if the branch
/// has moved since, so nothing is merged that was not on screen. A rebase
/// merge has no commit message, so the title is left out for it.
pub fn merge_pull(
    owner: &str,
    name: &str,
    number: u64,
    method: &str,
    commit_title: Option<&str>,
    sha: Option<&str>,
) -> ApiRequest {
    let mut body = json!({ "merge_method": method });
    if method != "rebase"
        && let Some(title) = commit_title.filter(|title| !title.is_empty())
    {
        body["commit_title"] = json!(title);
    }
    if let Some(sha) = sha.filter(|sha| !sha.is_empty()) {
        body["sha"] = json!(sha);
    }
    with_body(
        "PUT",
        repo_path(owner, name, &format!("/pulls/{number}/merge")),
        body,
    )
}

/// Deletes a branch. Its slashes stay slashes, because the ref is part of
/// the path.
pub fn delete_branch(owner: &str, name: &str, branch: &str) -> ApiRequest {
    let branch = branch.split('/').map(seg).collect::<Vec<_>>().join("/");
    delete(repo_path(owner, name, &format!("/git/refs/heads/{branch}")))
}

pub fn collaborators(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/collaborators?per_page=100"))
}

pub fn add_collaborator(owner: &str, name: &str, username: &str, permission: &str) -> ApiRequest {
    with_body(
        "PUT",
        repo_path(owner, name, &format!("/collaborators/{}", seg(username))),
        json!({ "permission": permission }),
    )
}

pub fn remove_collaborator(owner: &str, name: &str, username: &str) -> ApiRequest {
    delete(repo_path(
        owner,
        name,
        &format!("/collaborators/{}", seg(username)),
    ))
}

// ── Invitations ────────────────────────────────────────────────────────

pub fn repo_invitations() -> ApiRequest {
    get("/user/repository_invitations?per_page=100".to_string())
}

pub fn accept_repo_invitation(invitation_id: u64) -> ApiRequest {
    ApiRequest {
        method: "PATCH",
        path: format!("/user/repository_invitations/{invitation_id}"),
        body: None,
        accept: ApiResponseFormat::Json,
        rate_limit_resource: "core",
    }
}

pub fn decline_repo_invitation(invitation_id: u64) -> ApiRequest {
    delete(format!("/user/repository_invitations/{invitation_id}"))
}

pub fn accept_org_invitation(org: &str) -> ApiRequest {
    with_body(
        "PATCH",
        format!("/user/memberships/orgs/{}", seg(org)),
        json!({ "state": "active" }),
    )
}

pub fn decline_org_invitation(org: &str) -> ApiRequest {
    delete(format!("/user/memberships/orgs/{}", seg(org)))
}

// ── Releases and packages ──────────────────────────────────────────────

pub fn releases(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/releases?per_page=50"))
}

pub fn create_release(
    owner: &str,
    name: &str,
    tag_name: &str,
    title: Option<&str>,
    body: Option<&str>,
    draft: bool,
    prerelease: bool,
) -> ApiRequest {
    with_body(
        "POST",
        repo_path(owner, name, "/releases"),
        json!({
            "tag_name": tag_name,
            "name": title,
            "body": body,
            "draft": draft,
            "prerelease": prerelease,
        }),
    )
}

/// The package types GitHub Packages has. Its API lists one type per
/// request.
pub const PACKAGE_TYPES: [&str; 6] = ["container", "npm", "maven", "rubygems", "nuget", "pip"];

pub fn org_packages(org: &str, package_type: &str) -> ApiRequest {
    get(format!(
        "/orgs/{}/packages?per_page=100&package_type={}",
        seg(org),
        seg(package_type)
    ))
}

pub fn user_packages(user: &str, package_type: &str) -> ApiRequest {
    get(format!(
        "/users/{}/packages?per_page=100&package_type={}",
        seg(user),
        seg(package_type)
    ))
}

/// The three places a package's versions can live, depending on who owns
/// it: an organisation, the signed-in user, or another user. In the order
/// they are tried.
pub fn package_versions(owner: &str, package_type: &str, package: &str) -> [ApiRequest; 3] {
    let tail = format!(
        "/packages/{}/{}/versions?per_page=100",
        seg(package_type),
        seg(package)
    );
    [
        get(format!("/orgs/{}{tail}", seg(owner))),
        get(format!("/user{tail}")),
        get(format!("/users/{}{tail}", seg(owner))),
    ]
}

// ── Actions and deployments ────────────────────────────────────────────

pub fn workflow_runs(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/actions/runs?per_page=50&page=1"))
}

pub fn workflow_runs_for_branch(owner: &str, name: &str, branch: &str) -> ApiRequest {
    get(format!(
        "{}?branch={}&per_page=50&page=1",
        repo_path(owner, name, "/actions/runs"),
        seg(branch)
    ))
}

pub fn workflow_run(owner: &str, name: &str, run_id: u64) -> ApiRequest {
    get(repo_path(owner, name, &format!("/actions/runs/{run_id}")))
}

pub fn workflow_run_jobs(owner: &str, name: &str, run_id: u64) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        &format!("/actions/runs/{run_id}/jobs?per_page=100"),
    ))
}

pub fn deployments(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/deployments?per_page=50"))
}

// ── Insights ───────────────────────────────────────────────────────────

pub fn traffic_views(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/traffic/views"))
}

pub fn traffic_clones(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/traffic/clones"))
}

pub fn traffic_referrers(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/traffic/popular/referrers"))
}

pub fn traffic_paths(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/traffic/popular/paths"))
}

pub fn dependabot_alerts(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(owner, name, "/dependabot/alerts?per_page=100"))
}

pub fn secret_scanning_alerts(owner: &str, name: &str) -> ApiRequest {
    get(repo_path(
        owner,
        name,
        "/secret-scanning/alerts?per_page=100",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(request: &ApiRequest) -> String {
        format!("{} {}", request.method, request.path)
    }

    #[test]
    fn a_ref_search_is_one_graphql_query_with_its_text_as_a_variable() {
        let request = search_refs("o", "r", "login \"fix\"", 50);
        assert_eq!(line(&request), "POST /graphql");
        assert_eq!(request.rate_limit_resource, "graphql");
        let body = request.body.unwrap();
        // The text is a variable, never spliced into the query.
        assert_eq!(body["variables"]["query"], "login \"fix\"");
        assert_eq!(body["variables"]["first"], 50);
        assert!(body["query"].as_str().unwrap().contains("refs/heads/"));
    }

    #[test]
    fn a_request_can_be_asked_for_one_page() {
        // Paging already in the request is replaced, other parameters kept.
        assert_eq!(
            line(&issues("o", "r", "open").page(3, 10)),
            "GET /repos/o/r/issues?state=open&sort=updated&direction=desc&per_page=10&page=3"
        );
        assert_eq!(
            line(&workflow_runs("o", "r").page(2, 10)),
            "GET /repos/o/r/actions/runs?per_page=10&page=2"
        );
        // A request with no query string gets one.
        assert_eq!(
            line(&repo("o", "r").page(1, 10)),
            "GET /repos/o/r?per_page=10&page=1"
        );
        // Pages count from 1.
        assert_eq!(
            line(&tags("o", "r").page(0, 10)),
            "GET /repos/o/r/tags?per_page=10&page=1"
        );
        // The body and method are untouched.
        let paged = update_user(json!({ "bio": "x" })).page(2, 5);
        assert_eq!(paged.method, "PATCH");
        assert_eq!(paged.body, Some(json!({ "bio": "x" })));
    }

    #[test]
    fn account_requests() {
        assert_eq!(line(&current_user()), "GET /user");
        assert_eq!(line(&user("octocat")), "GET /users/octocat");
        assert_eq!(
            line(&org_memberships()),
            "GET /user/memberships/orgs?per_page=100"
        );
        assert_eq!(line(&org_detail("mentenaz")), "GET /orgs/mentenaz");
        assert_eq!(
            line(&org_members("mentenaz")),
            "GET /orgs/mentenaz/members?per_page=100"
        );

        let update = update_user(json!({ "bio": "hello" }));
        assert_eq!(line(&update), "PATCH /user");
        assert_eq!(update.body, Some(json!({ "bio": "hello" })));
    }

    #[test]
    fn your_own_repositories_and_an_organisations_are_different_endpoints() {
        assert_eq!(
            line(&repos("self")),
            "GET /user/repos?per_page=100&sort=updated&affiliation=owner"
        );
        assert_eq!(
            line(&repos("mentenaz")),
            "GET /orgs/mentenaz/repos?per_page=100&sort=updated"
        );
    }

    #[test]
    fn repository_requests() {
        assert_eq!(line(&repo("o", "r")), "GET /repos/o/r");
        assert_eq!(
            line(&branches("o", "r")),
            "GET /repos/o/r/branches?per_page=100"
        );
        assert_eq!(line(&tags("o", "r")), "GET /repos/o/r/tags?per_page=100");
        assert_eq!(
            line(&recent_commits("o", "r")),
            "GET /repos/o/r/commits?per_page=100"
        );
        assert_eq!(
            recent_commits_for_ref("o", "r", "feature/a").path,
            "/repos/o/r/commits?sha=feature%2Fa"
        );

        let update = update_repo("o", "r", json!({ "description": "d" }));
        assert_eq!(line(&update), "PATCH /repos/o/r");
        assert_eq!(update.body, Some(json!({ "description": "d" })));

        let topics = update_topics("o", "r", &["rust".to_string(), "gpui".to_string()]);
        assert_eq!(line(&topics), "PUT /repos/o/r/topics");
        assert_eq!(topics.body, Some(json!({ "names": ["rust", "gpui"] })));
    }

    #[test]
    fn creating_a_repository_goes_to_the_owner_named_in_the_options() {
        let mine = create_repo(&json!({ "name": "new", "private": true }));
        assert_eq!(line(&mine), "POST /user/repos");
        assert_eq!(mine.body, Some(json!({ "name": "new", "private": true })));

        // `owner` picks the organisation and is not part of what is sent.
        let orgs = create_repo(&json!({ "name": "new", "owner": "mentenaz" }));
        assert_eq!(line(&orgs), "POST /orgs/mentenaz/repos");
        assert_eq!(orgs.body, Some(json!({ "name": "new" })));
    }

    #[test]
    fn collaboration_requests() {
        assert_eq!(
            line(&issues("o", "r", "open")),
            "GET /repos/o/r/issues?state=open&per_page=100&sort=updated&direction=desc"
        );
        assert_eq!(
            line(&pulls("o", "r", "all")),
            "GET /repos/o/r/pulls?state=all&per_page=100&sort=updated&direction=desc"
        );
        assert_eq!(
            line(&issue_comments("o", "r", 42)),
            "GET /repos/o/r/issues/42/comments?per_page=100"
        );
        assert_eq!(
            line(&collaborators("o", "r")),
            "GET /repos/o/r/collaborators?per_page=100"
        );

        let pull = create_pull("o", "r", "feature", "main", "Add it", None);
        assert_eq!(line(&pull), "POST /repos/o/r/pulls");
        assert_eq!(
            pull.body,
            Some(json!({ "title": "Add it", "head": "feature", "base": "main", "body": null }))
        );

        let add = add_collaborator("o", "r", "octocat", "push");
        assert_eq!(line(&add), "PUT /repos/o/r/collaborators/octocat");
        assert_eq!(add.body, Some(json!({ "permission": "push" })));

        let remove = remove_collaborator("o", "r", "octocat");
        assert_eq!(line(&remove), "DELETE /repos/o/r/collaborators/octocat");
        assert_eq!(remove.body, None);
    }

    #[test]
    fn pull_request_actions() {
        assert_eq!(line(&pull("o", "r", 7)), "GET /repos/o/r/pulls/7");
        assert_eq!(
            line(&check_runs("o", "r", "abc123")),
            "GET /repos/o/r/commits/abc123/check-runs?per_page=100"
        );

        let close = update_pull("o", "r", 7, json!({ "state": "closed" }));
        assert_eq!(line(&close), "PATCH /repos/o/r/pulls/7");
        assert_eq!(close.body, Some(json!({ "state": "closed" })));

        let comment = create_issue_comment("o", "r", 7, "Looks good");
        assert_eq!(line(&comment), "POST /repos/o/r/issues/7/comments");
        assert_eq!(comment.body, Some(json!({ "body": "Looks good" })));
    }

    #[test]
    fn a_merge_names_the_method_and_the_head_that_was_shown() {
        let squash = merge_pull("o", "r", 7, "squash", Some("Add it (#7)"), Some("abc123"));
        assert_eq!(line(&squash), "PUT /repos/o/r/pulls/7/merge");
        assert_eq!(
            squash.body,
            Some(json!({
                "merge_method": "squash",
                "commit_title": "Add it (#7)",
                "sha": "abc123",
            }))
        );

        // A rebase has no commit message, so a title is not sent with it.
        let rebase = merge_pull("o", "r", 7, "rebase", Some("ignored"), None);
        assert_eq!(rebase.body, Some(json!({ "merge_method": "rebase" })));

        // An empty title leaves the default one to GitHub.
        let merge = merge_pull("o", "r", 7, "merge", Some(""), Some(""));
        assert_eq!(merge.body, Some(json!({ "merge_method": "merge" })));
    }

    #[test]
    fn deleting_a_branch_keeps_its_slashes_in_the_path() {
        let request = delete_branch("o", "r", "feature/a b");
        assert_eq!(
            line(&request),
            "DELETE /repos/o/r/git/refs/heads/feature/a%20b"
        );
        assert_eq!(request.body, None);
    }

    #[test]
    fn invitation_requests() {
        assert_eq!(
            line(&repo_invitations()),
            "GET /user/repository_invitations?per_page=100"
        );
        assert_eq!(
            line(&accept_repo_invitation(7)),
            "PATCH /user/repository_invitations/7"
        );
        assert_eq!(
            line(&decline_repo_invitation(7)),
            "DELETE /user/repository_invitations/7"
        );

        let accept = accept_org_invitation("mentenaz");
        assert_eq!(line(&accept), "PATCH /user/memberships/orgs/mentenaz");
        assert_eq!(accept.body, Some(json!({ "state": "active" })));
        assert_eq!(
            line(&decline_org_invitation("mentenaz")),
            "DELETE /user/memberships/orgs/mentenaz"
        );
    }

    #[test]
    fn release_requests() {
        assert_eq!(
            line(&releases("o", "r")),
            "GET /repos/o/r/releases?per_page=50"
        );

        let release = create_release("o", "r", "v1.0.0", Some("One"), None, false, true);
        assert_eq!(line(&release), "POST /repos/o/r/releases");
        assert_eq!(
            release.body,
            Some(json!({
                "tag_name": "v1.0.0",
                "name": "One",
                "body": null,
                "draft": false,
                "prerelease": true,
            }))
        );
    }

    #[test]
    fn package_requests() {
        assert_eq!(
            line(&org_packages("mentenaz", "npm")),
            "GET /orgs/mentenaz/packages?per_page=100&package_type=npm"
        );
        assert_eq!(
            line(&user_packages("octocat", "container")),
            "GET /users/octocat/packages?per_page=100&package_type=container"
        );

        let versions = package_versions("mentenaz", "npm", "left-pad");
        assert_eq!(
            versions.iter().map(line).collect::<Vec<_>>(),
            vec![
                "GET /orgs/mentenaz/packages/npm/left-pad/versions?per_page=100",
                "GET /user/packages/npm/left-pad/versions?per_page=100",
                "GET /users/mentenaz/packages/npm/left-pad/versions?per_page=100",
            ]
        );
    }

    #[test]
    fn actions_and_insight_requests() {
        assert_eq!(
            line(&workflow_runs("o", "r")),
            "GET /repos/o/r/actions/runs?per_page=50&page=1"
        );
        assert_eq!(
            line(&workflow_run("o", "r", 9)),
            "GET /repos/o/r/actions/runs/9"
        );
        assert_eq!(
            line(&workflow_run_jobs("o", "r", 9)),
            "GET /repos/o/r/actions/runs/9/jobs?per_page=100"
        );
        assert_eq!(
            line(&deployments("o", "r")),
            "GET /repos/o/r/deployments?per_page=50"
        );
        assert_eq!(
            line(&traffic_views("o", "r")),
            "GET /repos/o/r/traffic/views"
        );
        assert_eq!(
            line(&traffic_clones("o", "r")),
            "GET /repos/o/r/traffic/clones"
        );
        assert_eq!(
            line(&traffic_referrers("o", "r")),
            "GET /repos/o/r/traffic/popular/referrers"
        );
        assert_eq!(
            line(&traffic_paths("o", "r")),
            "GET /repos/o/r/traffic/popular/paths"
        );
        assert_eq!(
            line(&dependabot_alerts("o", "r")),
            "GET /repos/o/r/dependabot/alerts?per_page=100"
        );
        assert_eq!(
            line(&secret_scanning_alerts("o", "r")),
            "GET /repos/o/r/secret-scanning/alerts?per_page=100"
        );
    }

    #[test]
    fn w1_code_reader_requests_have_the_right_accepts() {
        let commit = commit("o", "r", "main");
        assert_eq!(commit.path, "/repos/o/r/commits/main");
        let detail = commit_detail("o", "r", "abc123");
        assert_eq!(detail.path, "/repos/o/r/commits/abc123");

        let tree = tree("o", "r", "tree-sha", true);
        assert_eq!(tree.path, "/repos/o/r/git/trees/tree-sha?recursive=true");
        let directory = tree_dir("o", "r", "directory-tree-sha");
        assert_eq!(
            directory.path,
            "/repos/o/r/git/trees/directory-tree-sha?recursive=false"
        );
        assert_eq!(tree.accept, ApiResponseFormat::Json);

        let blob = blob("o", "r", "abc123");
        assert_eq!(blob.path, "/repos/o/r/git/blobs/abc123");
        assert_eq!(blob.accept, ApiResponseFormat::Raw);

        let readme = readme("o", "r", "main");
        assert_eq!(readme.path, "/repos/o/r/readme?ref=main");
        assert_eq!(readme.accept, ApiResponseFormat::Json);
        let folder_readme = readme_at_path("o", "r", "docs/guide", "main");
        assert_eq!(
            folder_readme.path,
            "/repos/o/r/readme/docs%2Fguide?ref=main"
        );
        assert_eq!(folder_readme.accept, ApiResponseFormat::Json);
        assert_eq!(
            commits_for_path("o", "r", "docs/a file.md", "main").path,
            "/repos/o/r/commits?path=docs%2Fa%20file.md&sha=main"
        );

        let compare = compare("o", "r", "main", "feature");
        assert_eq!(compare.path, "/repos/o/r/compare/main...feature");
        assert_eq!(compare.accept, ApiResponseFormat::Json);
        assert_eq!(
            pull_for_head("o", "r", "o:feature/a").path,
            "/repos/o/r/pulls?state=all&head=o%3Afeature%2Fa&per_page=100&sort=updated&direction=desc"
        );
        assert_eq!(
            combined_status("o", "r", "abc123").path,
            "/repos/o/r/commits/abc123/status"
        );
        assert_eq!(
            workflow_runs_for_branch("o", "r", "feature/a").path,
            "/repos/o/r/actions/runs?branch=feature%2Fa&per_page=50&page=1"
        );

        let code = search_code("load_section_page", Some("mentenaz/zed_Source"), 1, 10);
        assert_eq!(
            code.path,
            "/search/code?q=repo%3Amentenaz%2Fzed_Source%20load_section_page&per_page=10&page=1"
        );
        assert_eq!(code.accept, ApiResponseFormat::TextMatch);
        assert_eq!(code.rate_limit_resource, "code_search");

        let repositories = search_repositories("gpui", 1, 10);
        assert_eq!(repositories.rate_limit_resource, "search");
        assert_eq!(tree.rate_limit_resource, "core");
    }

    #[test]
    fn a_name_cannot_change_which_endpoint_is_called() {
        // A value with path or query syntax in it stays one segment.
        assert_eq!(line(&user("a/b?c=d#e")), "GET /users/a%2Fb%3Fc%3Dd%23e");
        assert_eq!(
            line(&repo("o", "../../user")),
            "GET /repos/o/..%2F..%2Fuser"
        );
        assert_eq!(
            line(&issues("o", "r", "open&per_page=1")),
            "GET /repos/o/r/issues?state=open%26per_page%3D1&per_page=100&sort=updated&direction=desc"
        );
        assert_eq!(
            line(&remove_collaborator("o", "r", "x/../../y")),
            "DELETE /repos/o/r/collaborators/x%2F..%2F..%2Fy"
        );
        // Ordinary names are untouched.
        assert_eq!(
            line(&repo("mentenaz", "zed_Source")),
            "GET /repos/mentenaz/zed_Source"
        );
        assert_eq!(
            line(&repo("o", "Forge.Scaffold.SDK")),
            "GET /repos/o/Forge.Scaffold.SDK"
        );
    }

    #[test]
    fn a_container_package_keeps_its_slash_as_one_segment() {
        // Container images are commonly named `owner/image`.
        let versions = package_versions("mentenaz", "container", "forge/runner");
        assert_eq!(
            line(&versions[0]),
            "GET /orgs/mentenaz/packages/container/forge%2Frunner/versions?per_page=100"
        );
    }
}
