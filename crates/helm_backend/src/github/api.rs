//! GitHub REST API commands (was part of `github.rs`).
//!
//! `send` makes the request with the token the host handed to `GhState`; `error::interpret` turns the answer into a value or a
//! [`GhError`]. All endpoint functions are thin wrappers over those two.

use std::collections::VecDeque;
use std::time::Duration;

use base64::{Engine as _, prelude::BASE64_STANDARD};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpRequestExt as _, RedirectPolicy};
use serde::{Deserialize, de::DeserializeOwned};

use super::cache::{CachedResponse, RateLimit};
use super::error::{GhError, RawResponse, interpret, interpret_empty, unix_now};
use super::paging::{Page, interpret_page, interpret_page_under};
use super::requests::{self, ApiRequest, ApiResponseFormat};
use super::types::{
    BlobData, BlobKind, Branch, CheckRun, Collaborator, CombinedCommitStatus, Comment,
    CommitDetail, CommitFile, CommitSummary, CompareFile, CompareResult, Deployment, GhState,
    GitHubUser, GitHubUserDetail, Issue, MergeResult, OrgDetail, OrgInvitation, Package,
    PackageVersion, Pull, Readme,
    RefMovement, Release, Repo, RepoInvitation, RepoTree, ResolvedRef, SearchCodeMatch,
    SearchRepoResult, Tag, TrafficClones, TrafficPath, TrafficReferrer, TrafficViews, TreeEntry,
    TreeEntryKind, TreeLoadResult, WorkflowJob, WorkflowRun, classify_file, classify_ref_movement,
};

/// The access token, or the error a request without one would get.
async fn token(state: &GhState) -> Result<String, GhError> {
    state.token().await.ok_or_else(|| GhError::Unauthorized {
        message: "Not signed in to GitHub.".into(),
    })
}

/// How long a request may take, answer included, before it is given up on.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// GitHub answers a renamed or moved repository with a redirect.
const MAX_REDIRECTS: u32 = 10;

/// Makes one HTTP request and returns what came back, whatever its status,
/// together with the answer's `ETag`. This is the only function in the
/// crate that touches the network. What to ask for is `requests.rs`; what
/// the answer means is `error::interpret`.
///
/// It goes through the host application's HTTP client (`GhState::http`),
/// so it uses the same proxy settings as everything else the app sends.
/// `if_none_match` is sent as that header: GitHub then answers 304 with no
/// body when the data has not changed.
async fn send_once(
    state: &GhState,
    request: &ApiRequest,
    if_none_match: Option<&str>,
) -> Result<(RawResponse, Option<String>), GhError> {
    let token = token(state).await?;
    let _request_slot = state.acquire_request_slot().await.map_err(|error| {
        GhError::Other(format!("Could not acquire a GitHub request slot: {error}"))
    })?;
    let base = state.base_url.read().await.clone();
    let url = format!("{base}{}", request.path);
    let accept = match request.accept {
        ApiResponseFormat::Json => "application/vnd.github.v3+json",
        ApiResponseFormat::Raw => "application/vnd.github.raw+json",
        ApiResponseFormat::Diff => "application/vnd.github.diff",
        ApiResponseFormat::TextMatch => "application/vnd.github.text-match+json",
    };

    let mut builder = http_client::Request::builder()
        .method(request.method)
        .uri(url.as_str())
        .follow_redirects(RedirectPolicy::FollowLimit(MAX_REDIRECTS))
        .timeout(REQUEST_TIMEOUT)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", accept)
        .header("User-Agent", "mentenaz-forge");
    if let Some(etag) = if_none_match {
        builder = builder.header("If-None-Match", etag);
    }
    let body = match &request.body {
        Some(body) => {
            builder = builder.header("Content-Type", "application/json");
            AsyncBody::from(body.to_string())
        }
        None => AsyncBody::default(),
    };
    let http_request = builder
        .body(body)
        .map_err(|e| GhError::Other(format!("Could not build the request to {url}: {e}")))?;

    let mut response = state
        .http
        .send(http_request)
        .await
        .map_err(|e| GhError::Network(e.to_string()))?;
    let text = |name: &str| -> Option<String> {
        Some(
            response
                .headers()
                .get(name)?
                .to_str()
                .ok()?
                .trim()
                .to_string(),
        )
    };
    let number = |name: &str| -> Option<u64> { text(name)?.parse().ok() };
    let status = response.status().as_u16();
    let rate_remaining = number("x-ratelimit-remaining");
    let rate_reset = number("x-ratelimit-reset");
    let retry_after = number("retry-after");
    let link = text("link");
    let etag = text("etag");
    let github_sso = text("x-github-sso");
    // Classic OAuth tokens report their scopes on every answer; fine-grained
    // tokens send no such header, which leaves the scopes unknown.
    if let Some(scopes) = text("x-oauth-scopes") {
        state.record_token_scopes(parse_scopes(&scopes));
    }
    let rate = RateLimit::from_headers(number("x-ratelimit-limit"), rate_remaining, rate_reset);
    state.record_rate_limit(request.rate_limit_resource, rate, retry_after, unix_now());

    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| GhError::Network(e.to_string()))?;
    Ok((
        RawResponse {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            body_bytes: bytes,
            rate_remaining,
            rate_reset,
            retry_after,
            link,
            github_sso,
        },
        etag,
    ))
}

/// The scopes in an `X-OAuth-Scopes` header: a comma-separated list, empty
/// for a token with no scopes.
fn parse_scopes(header: &str) -> Vec<String> {
    header
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect()
}

/// Sends a request, using and keeping the remembered answer for it.
///
/// A `GET` whose answer is remembered is sent with that answer's tag. If
/// GitHub says nothing changed, the remembered answer is returned, and the
/// request did not count against the rate limit. Any other request that
/// succeeds changed something, so everything remembered is forgotten.
async fn send(state: &GhState, request: ApiRequest) -> Result<RawResponse, GhError> {
    // Once GitHub has said the allowance is used up, asking again before it
    // resets can only fail, and repeated failures can get a token blocked.
    let now = unix_now();
    let rate = state.rate_limit_for(request.rate_limit_resource);
    let retry_at = state.rate_limit_retry_at(request.rate_limit_resource);
    if rate.is_some_and(|rate| rate.is_used_up(now)) || retry_at.is_some_and(|until| now < until) {
        return Err(GhError::RateLimited {
            reset_at: rate.map(|rate| rate.reset_at),
            retry_after: retry_at.map(|until| until.saturating_sub(now)),
        });
    }

    let cacheable = request.method == "GET";
    let known = if cacheable {
        state.cache().etag(&request.path, request.accept)
    } else {
        None
    };

    let (mut response, mut etag) = send_once(state, &request, known.as_deref()).await?;
    if response.status == 304 {
        let cached = state.cache().get(&request.path, request.accept);
        if let Some(cached) = cached {
            return Ok(RawResponse {
                status: 200,
                body: String::from_utf8_lossy(&cached.body).into_owned(),
                body_bytes: cached.body,
                link: cached.link,
                ..response
            });
        }
        // The answer was forgotten while the request was out (a change was
        // sent in the meantime): ask again without the tag.
        (response, etag) = send_once(state, &request, None).await?;
    }

    if cacheable {
        if let (200, Some(etag)) = (response.status, etag) {
            state.cache().put(
                &request.path,
                request.accept,
                CachedResponse {
                    etag,
                    body: response.body_bytes.clone(),
                    link: response.link.clone(),
                },
            );
        }
    } else if response.status < 400 {
        state.forget_answers();
    }
    Ok(response)
}

/// Sends a request and reads its answer as `T`.
async fn fetch<T: DeserializeOwned>(state: &GhState, request: ApiRequest) -> Result<T, GhError> {
    interpret(&send(state, request).await?)
}

/// Branches and tags whose names matched a search ([`gh_search_refs`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefMatches {
    pub branches: Vec<String>,
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct GraphQlAnswer {
    data: Option<RefSearchData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RefSearchData {
    repository: Option<RefSearchRepository>,
}

#[derive(Deserialize)]
struct RefSearchRepository {
    branches: RefNodes,
    tags: RefNodes,
}

#[derive(Deserialize)]
struct RefNodes {
    nodes: Vec<RefNode>,
}

#[derive(Deserialize)]
struct RefNode {
    name: String,
}

/// What a ref search answered. GraphQL reports failures in an `errors` list
/// next to (or instead of) the data, with the HTTP status still 200, so they
/// are turned into the errors a REST answer would have given.
fn ref_matches(answer: GraphQlAnswer) -> Result<RefMatches, GhError> {
    if let Some(error) = answer.errors.into_iter().next() {
        let message = error.message;
        return Err(match error.kind.as_deref() {
            Some("NOT_FOUND") => GhError::NotFound { message },
            Some("FORBIDDEN") => GhError::Forbidden { message },
            _ => GhError::Other(format!("GitHub could not search the branches: {message}")),
        });
    }
    let repository = answer
        .data
        .and_then(|data| data.repository)
        .ok_or_else(|| GhError::NotFound {
            message: "Not Found".into(),
        })?;
    let names = |nodes: RefNodes| nodes.nodes.into_iter().map(|node| node.name).collect();
    Ok(RefMatches {
        branches: names(repository.branches),
        tags: names(repository.tags),
    })
}

/// Up to `first` branches and `first` tags of `owner/name` whose names
/// contain `query` anywhere, found by GitHub rather than among what was
/// loaded, so a repository with hundreds of branches can still be searched.
pub async fn gh_search_refs(
    owner: String,
    name: String,
    query: String,
    first: u32,
    state: &GhState,
) -> Result<RefMatches, GhError> {
    let answer: GraphQlAnswer =
        fetch(state, requests::search_refs(&owner, &name, &query, first)).await?;
    ref_matches(answer)
}

/// Sends a request whose answer carries nothing worth reading. GitHub
/// replies 204 with an empty body to most deletes and some updates.
async fn perform(state: &GhState, request: ApiRequest) -> Result<(), GhError> {
    interpret_empty(&send(state, request).await?)
}

/// One page of a list: page `page` (counting from 1) of `request`, at
/// `per_page` items a page. The result says which page is the last.
pub async fn fetch_page<T: DeserializeOwned>(
    state: &GhState,
    request: ApiRequest,
    page: u32,
    per_page: u32,
) -> Result<Page<T>, GhError> {
    let page = page.max(1);
    interpret_page(&send(state, request.page(page, per_page)).await?, page)
}

/// [`fetch_page`] for a list GitHub wraps in an object under `key`, as it
/// does for workflow runs.
pub async fn fetch_page_under<T: DeserializeOwned>(
    state: &GhState,
    request: ApiRequest,
    key: &str,
    page: u32,
    per_page: u32,
) -> Result<Page<T>, GhError> {
    let page = page.max(1);
    interpret_page_under(&send(state, request.page(page, per_page)).await?, key, page)
}

/// Page `page` of `request` as it was last answered, if that is remembered.
/// Nothing is sent. For showing a list at once while [`fetch_page`] checks
/// whether it has changed.
pub fn peek_page<T: DeserializeOwned>(
    state: &GhState,
    request: ApiRequest,
    page: u32,
    per_page: u32,
) -> Option<Page<T>> {
    let page = page.max(1);
    let remembered = state.remembered(&request.page(page, per_page))?;
    interpret_page(&remembered, page).ok()
}

/// [`peek_page`] for a list GitHub wraps in an object under `key`.
pub fn peek_page_under<T: DeserializeOwned>(
    state: &GhState,
    request: ApiRequest,
    key: &str,
    page: u32,
    per_page: u32,
) -> Option<Page<T>> {
    let page = page.max(1);
    let remembered = state.remembered(&request.page(page, per_page))?;
    interpret_page_under(&remembered, key, page).ok()
}

/// The most GitHub returns in one page.
const MAX_PER_PAGE: u32 = 100;
/// A stop for [`fetch_all`], so that a list that never ends cannot keep it
/// asking forever: 5,000 items.
const MAX_PAGES: u32 = 50;

/// Every page of a list, joined. For a list that has to be complete before
/// it is useful, such as one that is filtered locally. Stops after
/// [`MAX_PAGES`] pages.
pub async fn fetch_all<T: DeserializeOwned>(
    state: &GhState,
    request: ApiRequest,
) -> Result<Vec<T>, GhError> {
    let mut items = Vec::new();
    for page in 1..=MAX_PAGES {
        let fetched: Page<T> = fetch_page(state, request.clone(), page, MAX_PER_PAGE).await?;
        let more = fetched.has_next();
        items.extend(fetched.items);
        if !more {
            break;
        }
    }
    Ok(items)
}

/// The list GitHub wraps in an object, as it does for workflow runs
/// (`{"total_count": 3, "workflow_runs": [...]}`). Empty when the key is
/// missing or holds something else.
fn list_under<T: DeserializeOwned>(answer: &serde_json::Value, key: &str) -> Vec<T> {
    answer
        .get(key)
        .and_then(|list| serde_json::from_value(list.clone()).ok())
        .unwrap_or_default()
}

fn decode_text_content(value: &str, encoding: &str) -> String {
    match encoding {
        "base64" => {
            let bytes = BASE64_STANDARD
                .decode(value.replace('\n', ""))
                .unwrap_or_default();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        _ => value.to_string(),
    }
}

fn tree_kind(name: &str, mode: &str) -> TreeEntryKind {
    if mode == "120000" {
        return TreeEntryKind::Symlink;
    }
    if mode == "160000" {
        return TreeEntryKind::Submodule;
    }
    match name {
        "blob" => TreeEntryKind::Blob,
        "tree" => TreeEntryKind::Tree,
        "commit" => TreeEntryKind::Commit,
        "symlink" => TreeEntryKind::Symlink,
        "submodule" => TreeEntryKind::Submodule,
        _ => TreeEntryKind::Blob,
    }
}

/// Small JSON wrappers for the W1 endpoints. These are intentionally close to
/// GitHub's wire format so the workspace domain types remain stable.
#[derive(Deserialize)]
struct GitTreeEntryJson {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
    size: Option<u64>,
    target: Option<String>,
}

#[derive(Deserialize)]
struct GitTreeJson {
    sha: String,
    truncated: bool,
    tree: Vec<GitTreeEntryJson>,
}

#[derive(Deserialize)]
struct GitReadmeJson {
    name: String,
    path: String,
    html_url: String,
    content: String,
    encoding: String,
}

#[derive(Deserialize)]
struct GitCompareFileJson {
    filename: String,
    status: String,
    additions: u64,
    deletions: u64,
    patch: Option<String>,
}

#[derive(Deserialize)]
struct GitCompareJson {
    status: String,
    ahead_by: u64,
    behind_by: u64,
    commits: Vec<serde_json::Value>,
    files: Vec<GitCompareFileJson>,
}

#[derive(Deserialize)]
struct GitCommitDetailJson {
    sha: String,
    commit: GitCommitMessageJson,
    #[serde(default)]
    files: Vec<GitCommitFileJson>,
}

#[derive(Deserialize)]
struct GitCommitMessageJson {
    message: String,
}

#[derive(Deserialize)]
struct GitCommitFileJson {
    filename: String,
    status: String,
    additions: u64,
    deletions: u64,
    changes: u64,
    patch: Option<String>,
}

#[derive(Deserialize)]
struct GitSearchCodeItem {
    path: String,
    sha: Option<String>,
    repository: Option<serde_json::Value>,
    #[serde(default)]
    text_matches: Vec<serde_json::Value>,
}

/// The logins of the organisations the user is an active member of.
fn active_org_logins(memberships: &[serde_json::Value]) -> Vec<String> {
    memberships
        .iter()
        .filter(|membership| membership.get("state").and_then(|s| s.as_str()) == Some("active"))
        .filter_map(|membership| {
            membership
                .get("organization")
                .and_then(|organization| organization.get("login"))
                .and_then(|login| login.as_str())
                .map(str::to_string)
        })
        .collect()
}

/// The user's role in `org` ("admin" for an owner, or "member"), when they
/// are an active member of it. Logins are compared without regard to case,
/// as GitHub does.
fn org_role(memberships: &[serde_json::Value], org: &str) -> Option<String> {
    memberships
        .iter()
        .filter(|membership| membership.get("state").and_then(|s| s.as_str()) == Some("active"))
        .find(|membership| {
            membership
                .get("organization")
                .and_then(|organization| organization.get("login"))
                .and_then(|login| login.as_str())
                .is_some_and(|login| login.eq_ignore_ascii_case(org))
        })
        .and_then(|membership| membership.get("role"))
        .and_then(|role| role.as_str())
        .map(str::to_string)
}

/// The memberships the user has been offered and not yet answered.
fn pending_org_invitations(memberships: Vec<serde_json::Value>) -> Vec<OrgInvitation> {
    memberships
        .into_iter()
        .filter(|membership| membership.get("state").and_then(|s| s.as_str()) == Some("pending"))
        .filter_map(|membership| serde_json::from_value(membership).ok())
        .collect()
}

pub async fn fetch_repo_tree(
    state: &GhState,
    owner: &str,
    repo: &str,
    ref_name: &str,
) -> Result<TreeLoadResult, GhError> {
    let tree_sha = match resolve_tree_sha(state, owner, repo, ref_name).await {
        Ok(tree_sha) => tree_sha,
        Err(GhError::EmptyRepository) => return Ok(TreeLoadResult::EmptyRepository),
        Err(error) => return Err(error),
    };
    let tree: GitTreeJson = match fetch(state, requests::tree(owner, repo, &tree_sha, true)).await {
        Ok(tree) => tree,
        Err(GhError::EmptyRepository) => return Ok(TreeLoadResult::EmptyRepository),
        Err(error) => return Err(error),
    };
    if tree.truncated {
        return Ok(TreeLoadResult::Tree(
            fetch_truncated_tree(state, owner, repo, tree.sha).await?,
        ));
    }
    Ok(TreeLoadResult::Tree(map_tree(tree)))
}

async fn resolve_tree_sha(
    state: &GhState,
    owner: &str,
    repo: &str,
    ref_name: &str,
) -> Result<String, GhError> {
    Ok(resolve_ref(state, owner, repo, ref_name).await?.tree_sha)
}

pub async fn resolve_ref(
    state: &GhState,
    owner: &str,
    repo: &str,
    ref_name: &str,
) -> Result<ResolvedRef, GhError> {
    let commit: serde_json::Value = fetch(state, requests::commit(owner, repo, ref_name)).await?;
    resolved_ref(ref_name, &commit)
}

fn resolved_ref(ref_name: &str, commit: &serde_json::Value) -> Result<ResolvedRef, GhError> {
    let commit_sha = commit
        .get("sha")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| GhError::Parse("GitHub commit response did not contain sha".into()))?;
    let tree_sha = commit
        .pointer("/commit/tree/sha")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            GhError::Parse("GitHub commit response did not contain commit.tree.sha".into())
        })?;
    Ok(ResolvedRef {
        ref_name: ref_name.to_string(),
        commit_sha: commit_sha.to_string(),
        tree_sha: tree_sha.to_string(),
    })
}

pub async fn fetch_commit_detail(
    state: &GhState,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<CommitDetail, GhError> {
    let commit: GitCommitDetailJson =
        fetch(state, requests::commit_detail(owner, repo, sha)).await?;
    Ok(map_commit_detail(commit))
}

fn map_commit_detail(commit: GitCommitDetailJson) -> CommitDetail {
    CommitDetail {
        sha: commit.sha,
        message: commit.commit.message,
        files: commit
            .files
            .into_iter()
            .map(|file| CommitFile {
                path: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                changes: file.changes,
                patch: file.patch,
            })
            .collect(),
    }
}

pub async fn fetch_tree_dir(
    state: &GhState,
    owner: &str,
    repo: &str,
    tree_sha: &str,
) -> Result<TreeLoadResult, GhError> {
    let result: Result<GitTreeJson, GhError> =
        fetch(state, requests::tree_dir(owner, repo, tree_sha)).await;
    let tree = match result {
        Ok(tree) => tree,
        Err(error) => return map_tree_result(Err(error)),
    };
    if tree.truncated {
        return Ok(TreeLoadResult::Tree(
            fetch_truncated_tree(state, owner, repo, tree.sha).await?,
        ));
    }
    Ok(TreeLoadResult::Tree(map_tree(tree)))
}

fn map_tree_result(result: Result<GitTreeJson, GhError>) -> Result<TreeLoadResult, GhError> {
    match result {
        Ok(tree) => Ok(TreeLoadResult::Tree(map_tree(tree))),
        Err(GhError::EmptyRepository) => Ok(TreeLoadResult::EmptyRepository),
        Err(error) => Err(error),
    }
}

fn map_tree(tree: GitTreeJson) -> RepoTree {
    RepoTree {
        sha: tree.sha,
        truncated: tree.truncated,
        entries: tree
            .tree
            .into_iter()
            .map(|entry| TreeEntry {
                path: entry.path,
                kind: tree_kind(&entry.kind, &entry.mode),
                mode: entry.mode,
                sha: entry.sha,
                size: entry.size,
                target: entry.target,
            })
            .collect(),
    }
}

async fn fetch_truncated_tree(
    state: &GhState,
    owner: &str,
    repo: &str,
    root_sha: String,
) -> Result<RepoTree, GhError> {
    let mut pending = VecDeque::from([(root_sha.clone(), String::new())]);
    let mut entries = Vec::new();
    while let Some((tree_sha, prefix)) = pending.pop_front() {
        let tree: GitTreeJson = fetch(state, requests::tree_sha(owner, repo, &tree_sha)).await?;
        append_tree_directory(tree, &prefix, &mut pending, &mut entries)?;
    }
    Ok(map_tree(GitTreeJson {
        sha: root_sha,
        truncated: false,
        tree: entries,
    }))
}

fn append_tree_directory(
    tree: GitTreeJson,
    prefix: &str,
    pending: &mut VecDeque<(String, String)>,
    entries: &mut Vec<GitTreeEntryJson>,
) -> Result<(), GhError> {
    if tree.truncated {
        return Err(GhError::Other(format!(
            "Git tree {} was truncated even when fetched non-recursively",
            tree.sha
        )));
    }
    for mut entry in tree.tree {
        entry.path = if prefix.is_empty() {
            entry.path
        } else {
            format!("{prefix}/{}", entry.path)
        };
        if entry.kind == "tree" || entry.mode == "040000" {
            pending.push_back((entry.sha.clone(), entry.path.clone()));
        }
        entries.push(entry);
    }
    Ok(())
}

pub async fn fetch_blob(
    state: &GhState,
    owner: &str,
    repo: &str,
    sha: &str,
) -> Result<BlobData, GhError> {
    fetch_blob_at_path(state, owner, repo, "", sha, None).await
}

/// Fetch a blob with its tree path and known size. The known size lets files
/// beyond the inline limit be classified without downloading their contents.
pub async fn fetch_blob_at_path(
    state: &GhState,
    owner: &str,
    repo: &str,
    path: &str,
    sha: &str,
    expected_size: Option<u64>,
) -> Result<BlobData, GhError> {
    if let Some(size) = expected_size {
        let classification = classify_file(path, size, &[]);
        if classification.kind == BlobKind::TooLarge {
            return Ok(BlobData {
                sha: sha.to_string(),
                kind: classification.kind,
                size,
                content: Vec::new(),
                encoding: Some("raw".to_string()),
                lfs_size: classification.lfs_size,
            });
        }
    }
    let response = send(state, requests::blob(owner, repo, sha)).await?;
    interpret_empty(&response)?;
    Ok(blob_from_raw_response(path, sha, response.body_bytes))
}

fn blob_from_raw_response(path: &str, sha: &str, content: Vec<u8>) -> BlobData {
    let classification = classify_file(path, content.len() as u64, &content);
    BlobData {
        sha: sha.to_string(),
        kind: classification.kind,
        size: content.len() as u64,
        content,
        encoding: Some("raw".to_string()),
        lfs_size: classification.lfs_size,
    }
}

pub async fn fetch_readme(
    state: &GhState,
    owner: &str,
    repo: &str,
    ref_name: &str,
) -> Result<Readme, GhError> {
    let readme: GitReadmeJson = fetch(state, requests::readme(owner, repo, ref_name)).await?;
    Ok(map_readme(readme))
}

pub async fn fetch_readme_at_path(
    state: &GhState,
    owner: &str,
    repo: &str,
    path: &str,
    ref_name: &str,
) -> Result<Option<Readme>, GhError> {
    match fetch(state, requests::readme_at_path(owner, repo, path, ref_name)).await {
        Ok(readme) => Ok(Some(map_readme(readme))),
        Err(GhError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn map_readme(readme: GitReadmeJson) -> Readme {
    Readme {
        path: readme.path,
        name: readme.name,
        html_url: readme.html_url,
        content: decode_text_content(&readme.content, &readme.encoding),
        encoding: readme.encoding,
    }
}

pub async fn fetch_compare(
    state: &GhState,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<CompareResult, GhError> {
    let compare: GitCompareJson = fetch(state, requests::compare(owner, repo, base, head)).await?;
    Ok(CompareResult {
        status: compare.status,
        ahead_by: compare.ahead_by,
        behind_by: compare.behind_by,
        commits: compare
            .commits
            .into_iter()
            .filter_map(|value| serde_json::from_value::<CommitSummary>(value).ok())
            .collect(),
        files: compare
            .files
            .into_iter()
            .map(|file| CompareFile {
                path: file.filename,
                status: file.status,
                additions: file.additions,
                deletions: file.deletions,
                patch: file.patch,
            })
            .collect(),
    })
}

/// Compare the branch's previously displayed commit with its current commit.
/// `base...head` reports an advance when the new tip has no commits behind
/// the previous tip; otherwise the branch was rewritten or diverged.
pub async fn compare_ref_movement(
    state: &GhState,
    owner: &str,
    repo: &str,
    previous: &str,
    new: &str,
) -> Result<RefMovement, GhError> {
    if previous == new {
        return Ok(RefMovement::Unchanged);
    }
    let comparison = fetch_compare(state, owner, repo, previous, new).await?;
    Ok(classify_ref_movement(
        previous,
        new,
        comparison.ahead_by,
        comparison.behind_by,
    ))
}

pub async fn search_repositories(
    state: &GhState,
    query: &str,
    page: u32,
    per_page: u32,
) -> Result<Vec<SearchRepoResult>, GhError> {
    let answer: serde_json::Value =
        fetch(state, requests::search_repositories(query, page, per_page)).await?;
    let items = answer
        .get("items")
        .and_then(|items| items.as_array())
        .cloned()
        .unwrap_or_default();
    let mut repos = Vec::new();
    for item in items {
        if let Ok(repo) = serde_json::from_value::<Repo>(item.clone()) {
            repos.push(SearchRepoResult { repo, score: None });
        }
    }
    Ok(repos)
}

pub async fn search_repositories_page(
    state: &GhState,
    query: &str,
    page: u32,
    per_page: u32,
) -> Result<Page<Repo>, GhError> {
    #[derive(Deserialize)]
    struct SearchResponse {
        total_count: u64,
        items: Vec<Repo>,
    }

    let per_page = per_page.max(1);
    let answer: SearchResponse =
        fetch(state, requests::search_repositories(query, page, per_page)).await?;
    let last_page = search_last_page(answer.total_count, per_page);
    Ok(Page {
        items: answer.items,
        page: page.max(1),
        last_page,
    })
}

fn search_last_page(total_count: u64, per_page: u32) -> u32 {
    u32::try_from(total_count.div_ceil(u64::from(per_page.max(1))))
        .unwrap_or(u32::MAX)
        .clamp(1, 50)
}

pub async fn search_code(
    state: &GhState,
    query: &str,
    repo: Option<&str>,
    page: u32,
    per_page: u32,
) -> Result<Vec<SearchCodeMatch>, GhError> {
    let answer: serde_json::Value =
        fetch(state, requests::search_code(query, repo, page, per_page)).await?;
    Ok(parse_code_search_items(&answer))
}

pub async fn search_code_page(
    state: &GhState,
    query: &str,
    repo: Option<&str>,
    page: u32,
    per_page: u32,
) -> Result<Page<SearchCodeMatch>, GhError> {
    let answer: serde_json::Value =
        fetch(state, requests::search_code(query, repo, page, per_page)).await?;
    let total_count = answer
        .get("total_count")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| GhError::Parse("Code search response has no total_count.".into()))?;
    Ok(Page {
        items: parse_code_search_items(&answer),
        page: page.max(1),
        last_page: search_last_page(total_count, per_page),
    })
}

fn parse_code_search_items(answer: &serde_json::Value) -> Vec<SearchCodeMatch> {
    let items = answer
        .get("items")
        .and_then(|items| items.as_array())
        .cloned()
        .unwrap_or_default();
    let mut matches = Vec::new();
    for item in items {
        let parsed: GitSearchCodeItem = match serde_json::from_value(item.clone()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let repo_name = parsed
            .repository
            .clone()
            .and_then(|value| value.get("full_name").cloned())
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();
        matches.push(SearchCodeMatch {
            path: parsed.path,
            repo: repo_name,
            sha: parsed.sha,
            line: parsed
                .text_matches
                .first()
                .and_then(|value| value.get("fragment"))
                .and_then(|value| value.get("text"))
                .and_then(|value| value.as_str())
                .map(str::to_string),
            line_number: None,
            matches: Vec::new(),
        });
    }
    matches
}

pub async fn gh_get_current_user(state: &GhState) -> Result<GitHubUser, GhError> {
    fetch(state, requests::current_user()).await
}

/// The signed-in user's verified email addresses, the primary one first.
/// Fails with a permission error for a token without the `user` scope.
pub async fn gh_get_verified_emails(state: &GhState) -> Result<Vec<String>, GhError> {
    #[derive(Deserialize)]
    struct UserEmail {
        email: String,
        #[serde(default)]
        primary: bool,
        #[serde(default)]
        verified: bool,
    }
    let mut emails: Vec<UserEmail> = fetch(state, requests::current_user_emails()).await?;
    emails.retain(|email| email.verified);
    emails.sort_by_key(|email| !email.primary);
    Ok(emails.into_iter().map(|email| email.email).collect())
}

pub async fn gh_get_repos(owner: String, state: &GhState) -> Result<Vec<Repo>, GhError> {
    // Every page: the repository list is filtered by a search box, which
    // only works over the whole list.
    fetch_all(state, requests::repos(&owner)).await
}

pub async fn gh_get_repo(owner: String, name: String, state: &GhState) -> Result<Repo, GhError> {
    fetch(state, requests::repo(&owner, &name)).await
}

/// Recent commits with their GitHub author/committer info (login + avatar
/// URL), for mapping a local `git log` SHA to a real avatar in the Git Ops
/// History tab. One list call covers up to 100 commits, instead of hitting
/// `/repos/{owner}/{repo}/commits/{sha}` once per commit.
pub async fn gh_list_recent_commits(
    owner: String,
    name: String,
    state: &GhState,
) -> Result<Vec<CommitSummary>, GhError> {
    fetch(state, requests::recent_commits(&owner, &name)).await
}

pub async fn gh_create_repo(opts: serde_json::Value, state: &GhState) -> Result<Repo, GhError> {
    fetch(state, requests::create_repo(&opts)).await
}

pub async fn gh_get_branches(
    owner: String,
    name: String,
    state: &GhState,
) -> Result<Vec<Branch>, GhError> {
    fetch(state, requests::branches(&owner, &name)).await
}

pub async fn gh_update_repo(
    owner: String,
    name: String,
    changes: serde_json::Value,
    state: &GhState,
) -> Result<Repo, GhError> {
    fetch(state, requests::update_repo(&owner, &name, changes)).await
}

pub async fn gh_get_org_detail(org: String, state: &GhState) -> Result<OrgDetail, GhError> {
    fetch(state, requests::org_detail(&org)).await
}

/// The signed-in user's role in `org`, or `None` when they are not an
/// active member. Asks for the same membership list sign-in does, so the
/// answer is usually one GitHub has already given.
pub async fn gh_get_org_role(org: &str, state: &GhState) -> Result<Option<String>, GhError> {
    let memberships: Vec<serde_json::Value> = fetch(state, requests::org_memberships()).await?;
    Ok(org_role(&memberships, org))
}

pub async fn gh_get_collaborators(
    owner: String,
    name: String,
    state: &GhState,
) -> Result<Vec<Collaborator>, GhError> {
    fetch(state, requests::collaborators(&owner, &name)).await
}

pub async fn gh_get_org_logins(state: &GhState) -> Result<Vec<String>, GhError> {
    let memberships: Vec<serde_json::Value> = fetch(state, requests::org_memberships()).await?;
    Ok(active_org_logins(&memberships))
}

/// Same endpoint/shape as [`gh_get_org_logins`], filtered to `state ==
/// "pending"` instead of `"active"` — the memberships GitHub hasn't been
/// accepted or declined yet.
pub async fn gh_list_org_invitations(state: &GhState) -> Result<Vec<OrgInvitation>, GhError> {
    let memberships: Vec<serde_json::Value> = fetch(state, requests::org_memberships()).await?;
    Ok(pending_org_invitations(memberships))
}

pub async fn gh_get_user(username: String, state: &GhState) -> Result<GitHubUserDetail, GhError> {
    fetch(state, requests::user(&username)).await
}

pub async fn gh_list_workflow_runs(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<WorkflowRun>, GhError> {
    let answer: serde_json::Value = fetch(state, requests::workflow_runs(&owner, &repo)).await?;
    Ok(list_under(&answer, "workflow_runs"))
}

/// A single run's own current status/conclusion — used to refresh the
/// header of the run-detail screen on each poll tick, since `html_url` etc.
/// never change but `status`/`conclusion` do while it's running.
pub async fn gh_get_workflow_run(
    owner: String,
    repo: String,
    run_id: u64,
    state: &GhState,
) -> Result<WorkflowRun, GhError> {
    fetch(state, requests::workflow_run(&owner, &repo, run_id)).await
}

/// Per-job (and per-step within each job) status for a run — the detail
/// `gh_list_workflow_runs`/`WorkflowRun` doesn't carry at all. This is what
/// actually answers "what is it doing right now": each step's own
/// status/conclusion, not just the run as a whole.
pub async fn gh_get_workflow_run_jobs(
    owner: String,
    repo: String,
    run_id: u64,
    state: &GhState,
) -> Result<Vec<WorkflowJob>, GhError> {
    let answer: serde_json::Value =
        fetch(state, requests::workflow_run_jobs(&owner, &repo, run_id)).await?;
    Ok(list_under(&answer, "jobs"))
}

pub async fn gh_list_deployments(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<Deployment>, GhError> {
    fetch(state, requests::deployments(&owner, &repo)).await
}

pub async fn gh_list_issues(
    owner: String,
    repo: String,
    state_filter: String,
    state: &GhState,
) -> Result<Vec<Issue>, GhError> {
    fetch(state, requests::issues(&owner, &repo, &state_filter)).await
}

pub async fn gh_list_pulls(
    owner: String,
    repo: String,
    state_filter: String,
    state: &GhState,
) -> Result<Vec<Pull>, GhError> {
    fetch(state, requests::pulls(&owner, &repo, &state_filter)).await
}

pub async fn gh_get_combined_status(
    owner: &str,
    repo: &str,
    ref_name: &str,
    state: &GhState,
) -> Result<CombinedCommitStatus, GhError> {
    fetch(state, requests::combined_status(owner, repo, ref_name)).await
}

/// Comments on an issue or PR (see [`Comment`]'s doc comment for why one
/// endpoint covers both) — the in-panel detail viewer's discussion thread.
/// `number` is the issue/PR number, same one already shown on its list row.
pub async fn gh_list_issue_comments(
    owner: String,
    repo: String,
    number: u64,
    state: &GhState,
) -> Result<Vec<Comment>, GhError> {
    fetch(state, requests::issue_comments(&owner, &repo, number)).await
}

pub async fn gh_list_releases(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<Release>, GhError> {
    fetch(state, requests::releases(&owner, &repo)).await
}

pub async fn gh_list_tags(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<Tag>, GhError> {
    fetch(state, requests::tags(&owner, &repo)).await
}

pub async fn gh_create_pull(
    owner: String,
    repo: String,
    head: String,
    base: String,
    title: String,
    body: Option<String>,
    state: &GhState,
) -> Result<serde_json::Value, GhError> {
    fetch(
        state,
        requests::create_pull(&owner, &repo, &head, &base, &title, body.as_deref()),
    )
    .await
}

/// One pull request on its own: unlike the list, it says whether it can be
/// merged (`mergeable_state`). GitHub works that out when first asked, so
/// `mergeable` can come back `None`; ask again shortly after.
pub async fn gh_get_pull(
    owner: String,
    repo: String,
    number: u64,
    state: &GhState,
) -> Result<Pull, GhError> {
    fetch(state, requests::pull(&owner, &repo, number)).await
}

/// Closes, reopens or edits a pull request. See [`requests::update_pull`]
/// for what `changes` may hold.
pub async fn gh_update_pull(
    owner: String,
    repo: String,
    number: u64,
    changes: serde_json::Value,
    state: &GhState,
) -> Result<Pull, GhError> {
    fetch(state, requests::update_pull(&owner, &repo, number, changes)).await
}

/// Posts a comment on an issue or a pull request.
pub async fn gh_create_issue_comment(
    owner: String,
    repo: String,
    number: u64,
    body: String,
    state: &GhState,
) -> Result<Comment, GhError> {
    fetch(
        state,
        requests::create_issue_comment(&owner, &repo, number, &body),
    )
    .await
}

/// The checks that ran on `ref_name` (a commit SHA).
pub async fn gh_list_check_runs(
    owner: String,
    repo: String,
    ref_name: String,
    state: &GhState,
) -> Result<Vec<CheckRun>, GhError> {
    let answer: serde_json::Value =
        fetch(state, requests::check_runs(&owner, &repo, &ref_name)).await?;
    Ok(list_under(&answer, "check_runs"))
}

/// Merges a pull request. See [`requests::merge_pull`]. GitHub answers 405
/// when the pull request cannot be merged and 409 when `sha` is no longer the
/// head; both come back as errors that carry GitHub's own message.
pub async fn gh_merge_pull(
    owner: String,
    repo: String,
    number: u64,
    method: String,
    commit_title: Option<String>,
    sha: Option<String>,
    state: &GhState,
) -> Result<MergeResult, GhError> {
    fetch(
        state,
        requests::merge_pull(
            &owner,
            &repo,
            number,
            &method,
            commit_title.as_deref(),
            sha.as_deref(),
        ),
    )
    .await
}

pub async fn gh_delete_branch(
    owner: String,
    repo: String,
    branch: String,
    state: &GhState,
) -> Result<(), GhError> {
    perform(state, requests::delete_branch(&owner, &repo, &branch)).await
}

pub async fn gh_create_release(
    owner: String,
    repo: String,
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    draft: Option<bool>,
    prerelease: Option<bool>,
    state: &GhState,
) -> Result<serde_json::Value, GhError> {
    fetch(
        state,
        requests::create_release(
            &owner,
            &repo,
            &tag_name,
            name.as_deref(),
            body.as_deref(),
            draft.unwrap_or(false),
            prerelease.unwrap_or(false),
        ),
    )
    .await
}

pub async fn gh_list_dependabot_alerts(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<serde_json::Value>, GhError> {
    fetch(state, requests::dependabot_alerts(&owner, &repo)).await
}

pub async fn gh_list_secret_scanning_alerts(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<serde_json::Value>, GhError> {
    fetch(state, requests::secret_scanning_alerts(&owner, &repo)).await
}

pub async fn gh_get_traffic_views(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<TrafficViews, GhError> {
    fetch(state, requests::traffic_views(&owner, &repo)).await
}

pub async fn gh_get_traffic_clones(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<TrafficClones, GhError> {
    fetch(state, requests::traffic_clones(&owner, &repo)).await
}

pub async fn gh_get_traffic_referrers(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<TrafficReferrer>, GhError> {
    fetch(state, requests::traffic_referrers(&owner, &repo)).await
}

pub async fn gh_get_traffic_paths(
    owner: String,
    repo: String,
    state: &GhState,
) -> Result<Vec<TrafficPath>, GhError> {
    fetch(state, requests::traffic_paths(&owner, &repo)).await
}

pub async fn gh_list_packages(owner: String, state: &GhState) -> Result<Vec<Package>, GhError> {
    // The API only accepts one `package_type` per request, so run one call
    // per type and merge. The org path is tried first (org-owner repos), the
    // user path as fallback (same login, individual account).
    let mut combined: Vec<Package> = Vec::new();
    let mut last_err: Option<GhError> = None;
    for package_type in requests::PACKAGE_TYPES {
        let from_org = fetch::<Vec<Package>>(state, requests::org_packages(&owner, package_type));
        let result = match from_org.await {
            Ok(packages) => Ok(packages),
            Err(org_err) => {
                match fetch::<Vec<Package>>(state, requests::user_packages(&owner, package_type))
                    .await
                {
                    Ok(packages) => Ok(packages),
                    Err(_) => Err(org_err),
                }
            }
        };
        match result {
            Ok(packages) => combined.extend(packages),
            Err(e) => last_err = Some(e),
        }
    }
    if combined.is_empty() {
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    Ok(combined)
}

pub async fn gh_list_package_versions(
    owner: String,
    pkg_type: String,
    pkg_name: String,
    state: &GhState,
) -> Result<Vec<PackageVersion>, GhError> {
    // The versions endpoint lives under a different prefix depending on who
    // owns the package (org, the authenticated user, or another user) — try
    // each in turn, first success wins.
    let mut last_err = None;
    for request in requests::package_versions(&owner, &pkg_type, &pkg_name) {
        match fetch::<Vec<PackageVersion>>(state, request).await {
            Ok(versions) => return Ok(versions),
            Err(e) => last_err = Some(e),
        }
    }
    // There are always three places to try, so there is a last error.
    Err(last_err.unwrap_or(GhError::NotFound {
        message: "no package versions found".to_string(),
    }))
}

pub async fn gh_update_user(
    changes: serde_json::Value,
    state: &GhState,
) -> Result<serde_json::Value, GhError> {
    fetch(state, requests::update_user(changes)).await
}

pub async fn gh_get_repo_invitations(state: &GhState) -> Result<Vec<RepoInvitation>, GhError> {
    fetch(state, requests::repo_invitations()).await
}

pub async fn gh_accept_repo_invitation(invitation_id: u64, state: &GhState) -> Result<(), GhError> {
    perform(state, requests::accept_repo_invitation(invitation_id)).await
}

pub async fn gh_decline_repo_invitation(
    invitation_id: u64,
    state: &GhState,
) -> Result<(), GhError> {
    perform(state, requests::decline_repo_invitation(invitation_id)).await
}

pub async fn gh_accept_org_invitation(org: String, state: &GhState) -> Result<(), GhError> {
    perform(state, requests::accept_org_invitation(&org)).await
}

pub async fn gh_decline_org_invitation(org: String, state: &GhState) -> Result<(), GhError> {
    perform(state, requests::decline_org_invitation(&org)).await
}

pub async fn gh_add_collaborator(
    owner: String,
    repo: String,
    username: String,
    permission: String,
    state: &GhState,
) -> Result<(), GhError> {
    perform(
        state,
        requests::add_collaborator(&owner, &repo, &username, &permission),
    )
    .await
}

pub async fn gh_remove_collaborator(
    owner: String,
    repo: String,
    username: String,
    state: &GhState,
) -> Result<(), GhError> {
    perform(
        state,
        requests::remove_collaborator(&owner, &repo, &username),
    )
    .await
}

pub async fn gh_update_topics(
    owner: String,
    repo: String,
    topics: Vec<String>,
    state: &GhState,
) -> Result<serde_json::Value, GhError> {
    fetch(state, requests::update_topics(&owner, &repo, &topics)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_ref_search_answer_becomes_branch_and_tag_names() {
        let answer: GraphQlAnswer = serde_json::from_value(json!({
            "data": { "repository": {
                "branches": { "nodes": [{ "name": "feature/login-fix" }, { "name": "login" }] },
                "tags": { "nodes": [{ "name": "login-v1" }] }
            }}
        }))
        .unwrap();
        assert_eq!(
            ref_matches(answer).unwrap(),
            RefMatches {
                branches: vec!["feature/login-fix".into(), "login".into()],
                tags: vec!["login-v1".into()],
            }
        );

        // A repository GitHub does not show is not found.
        let missing: GraphQlAnswer = serde_json::from_value(json!({
            "data": { "repository": null },
            "errors": [{ "type": "NOT_FOUND", "message": "Could not resolve to a Repository" }]
        }))
        .unwrap();
        assert!(matches!(ref_matches(missing), Err(GhError::NotFound { .. })));
    }

    #[test]
    fn oauth_scopes_header_is_split_and_trimmed() {
        assert_eq!(
            parse_scopes("repo, read:org,user"),
            vec!["repo", "read:org", "user"]
        );
        assert!(parse_scopes("").is_empty());
        assert!(parse_scopes(" , ").is_empty());
    }

    #[test]
    fn search_paging_uses_total_count_and_github_limit() {
        assert_eq!(search_last_page(0, 20), 1);
        assert_eq!(search_last_page(20, 20), 1);
        assert_eq!(search_last_page(21, 20), 2);
        assert_eq!(search_last_page(10_001, 20), 50);
    }

    fn memberships() -> Vec<serde_json::Value> {
        vec![
            json!({ "state": "active", "role": "admin", "organization": { "login": "mentenaz" } }),
            json!({ "state": "pending", "role": "member", "organization": { "login": "invited" } }),
            json!({ "state": "active", "role": "member", "organization": { "login": "other" } }),
            // Malformed entries are skipped, not fatal.
            json!({ "state": "active" }),
            json!({ "state": "pending" }),
        ]
    }

    #[test]
    fn active_memberships_are_the_users_organisations() {
        assert_eq!(active_org_logins(&memberships()), vec!["mentenaz", "other"]);
        assert!(active_org_logins(&[]).is_empty());
    }

    #[test]
    fn a_role_is_given_only_for_an_active_membership() {
        let memberships = memberships();
        assert_eq!(org_role(&memberships, "mentenaz").as_deref(), Some("admin"));
        assert_eq!(org_role(&memberships, "Other").as_deref(), Some("member"));
        // Invited but not yet accepted, and not a member at all.
        assert_eq!(org_role(&memberships, "invited"), None);
        assert_eq!(org_role(&memberships, "elsewhere"), None);
    }

    #[test]
    fn pending_memberships_are_invitations() {
        let invitations = pending_org_invitations(memberships());
        assert_eq!(invitations.len(), 1);
        assert_eq!(invitations[0].organization.login, "invited");
        assert_eq!(invitations[0].role, "member");
    }

    #[test]
    fn a_wrapped_list_is_unwrapped() {
        let answer = json!({ "total_count": 2, "items": [1, 2] });
        assert_eq!(list_under::<u32>(&answer, "items"), vec![1, 2]);
        // A missing key, or one holding something else, is an empty list.
        assert!(list_under::<u32>(&answer, "missing").is_empty());
        assert!(list_under::<u32>(&json!({ "items": "nope" }), "items").is_empty());
    }

    #[test]
    fn raw_blob_mapping_preserves_non_utf8_bytes() {
        let bytes = vec![0x00, 0xFF, 0x80, b'\n'];
        let blob = blob_from_raw_response("asset.bin", "blob-sha", bytes.clone());
        assert_eq!(blob.sha, "blob-sha");
        assert_eq!(blob.kind, BlobKind::Binary);
        assert_eq!(blob.content, bytes);
        assert_eq!(blob.size, 4);
        assert_eq!(blob.encoding.as_deref(), Some("raw"));

        let text = blob_from_raw_response("main.rs", "text-sha", b"source\n".to_vec());
        assert_eq!(text.kind, BlobKind::Text);
        assert_eq!(text.content, b"source\n");

        let image = blob_from_raw_response(
            "image.png",
            "image-sha",
            b"\x89PNG\r\n\x1a\ncontent".to_vec(),
        );
        assert_eq!(image.kind, BlobKind::Image);

        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 999\n";
        let lfs = blob_from_raw_response("asset.dat", "lfs-sha", pointer.to_vec());
        assert_eq!(lfs.kind, BlobKind::LfsPointer);
        assert_eq!(lfs.lfs_size, Some(999));
    }

    #[test]
    fn tree_sha_request_is_non_recursive() {
        let request = requests::tree_sha("o", "r", "tree123");
        assert_eq!(request.path, "/repos/o/r/git/trees/tree123?recursive=false");
    }

    #[test]
    fn commit_response_resolves_both_commit_and_tree_sha() {
        assert_eq!(
            resolved_ref(
                "main",
                &json!({
                    "sha": "commit-sha",
                    "commit": { "tree": { "sha": "tree-sha" } }
                })
            ),
            Ok(ResolvedRef {
                ref_name: "main".into(),
                commit_sha: "commit-sha".into(),
                tree_sha: "tree-sha".into(),
            })
        );
        assert!(matches!(
            resolved_ref("main", &json!({ "sha": "commit-sha", "commit": {} })),
            Err(GhError::Parse(_))
        ));
    }

    #[test]
    fn commit_detail_mapping_keeps_file_stats_and_optional_patches() {
        let raw: GitCommitDetailJson = serde_json::from_value(json!({
            "sha": "commit-sha",
            "commit": { "message": "Add file\n\nDetails" },
            "files": [
                {
                    "filename": "src/main.rs",
                    "status": "modified",
                    "additions": 2,
                    "deletions": 1,
                    "changes": 3,
                    "patch": "@@ -1 +1,2 @@"
                },
                {
                    "filename": "asset.bin",
                    "status": "added",
                    "additions": 0,
                    "deletions": 0,
                    "changes": 0
                }
            ]
        }))
        .unwrap();
        assert_eq!(
            map_commit_detail(raw),
            CommitDetail {
                sha: "commit-sha".into(),
                message: "Add file\n\nDetails".into(),
                files: vec![
                    CommitFile {
                        path: "src/main.rs".into(),
                        status: "modified".into(),
                        additions: 2,
                        deletions: 1,
                        changes: 3,
                        patch: Some("@@ -1 +1,2 @@".into()),
                    },
                    CommitFile {
                        path: "asset.bin".into(),
                        status: "added".into(),
                        additions: 0,
                        deletions: 0,
                        changes: 0,
                        patch: None,
                    },
                ],
            }
        );
    }

    #[test]
    fn compare_payload_classifies_ref_movement() {
        let forward = json!({ "ahead_by": 3, "behind_by": 0 });
        assert_eq!(
            classify_ref_movement(
                "old",
                "new",
                forward["ahead_by"].as_u64().unwrap(),
                forward["behind_by"].as_u64().unwrap(),
            ),
            RefMovement::Advanced { commits: 3 }
        );
        assert_eq!(
            classify_ref_movement("same", "same", 0, 0),
            RefMovement::Unchanged
        );
        assert_eq!(
            classify_ref_movement("old", "new", 0, 2),
            RefMovement::Rewritten
        );
        assert_eq!(
            classify_ref_movement("old", "new", 1, 1),
            RefMovement::Rewritten
        );
    }

    #[test]
    fn tree_mapping_preserves_truncation_and_entry_kinds() {
        assert_eq!(
            map_tree_result(Err(GhError::EmptyRepository)),
            Ok(TreeLoadResult::EmptyRepository)
        );

        let tree = GitTreeJson {
            sha: "tree-sha".into(),
            truncated: true,
            tree: vec![
                GitTreeEntryJson {
                    path: "link".into(),
                    mode: "120000".into(),
                    kind: "blob".into(),
                    sha: "symlink-sha".into(),
                    size: Some(4),
                    target: None,
                },
                GitTreeEntryJson {
                    path: "vendor".into(),
                    mode: "160000".into(),
                    kind: "commit".into(),
                    sha: "submodule-sha".into(),
                    size: None,
                    target: None,
                },
            ],
        };
        assert_eq!(
            map_tree_result(Ok(tree)),
            Ok(TreeLoadResult::Tree(RepoTree {
                sha: "tree-sha".into(),
                truncated: true,
                entries: vec![
                    TreeEntry {
                        path: "link".into(),
                        mode: "120000".into(),
                        kind: TreeEntryKind::Symlink,
                        sha: "symlink-sha".into(),
                        size: Some(4),
                        target: None,
                    },
                    TreeEntry {
                        path: "vendor".into(),
                        mode: "160000".into(),
                        kind: TreeEntryKind::Submodule,
                        sha: "submodule-sha".into(),
                        size: None,
                        target: None,
                    },
                ],
            }))
        );

        let forbidden = GhError::Forbidden {
            message: "Resource not accessible".into(),
        };
        assert_eq!(map_tree_result(Err(forbidden.clone())), Err(forbidden));
    }

    #[test]
    fn truncated_tree_fallback_walks_child_tree_shas_and_prefixes_paths() {
        let mut pending = VecDeque::new();
        let mut entries = Vec::new();
        append_tree_directory(
            GitTreeJson {
                sha: "root".into(),
                truncated: false,
                tree: vec![
                    GitTreeEntryJson {
                        path: "src".into(),
                        mode: "040000".into(),
                        kind: "tree".into(),
                        sha: "src-tree".into(),
                        size: None,
                        target: None,
                    },
                    GitTreeEntryJson {
                        path: "vendor".into(),
                        mode: "160000".into(),
                        kind: "commit".into(),
                        sha: "submodule".into(),
                        size: None,
                        target: None,
                    },
                ],
            },
            "",
            &mut pending,
            &mut entries,
        )
        .unwrap();
        assert_eq!(pending, VecDeque::from([("src-tree".into(), "src".into())]));

        let (sha, prefix) = pending.pop_front().unwrap();
        assert_eq!(sha, "src-tree");
        append_tree_directory(
            GitTreeJson {
                sha,
                truncated: false,
                tree: vec![GitTreeEntryJson {
                    path: "main.rs".into(),
                    mode: "100644".into(),
                    kind: "blob".into(),
                    sha: "file-sha".into(),
                    size: Some(10),
                    target: None,
                }],
            },
            &prefix,
            &mut pending,
            &mut entries,
        )
        .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src", "vendor", "src/main.rs"]
        );
        assert!(pending.is_empty());
    }
}
