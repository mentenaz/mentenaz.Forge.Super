//! GitHub data types and shared state.

use serde::{Deserialize, Serialize};

use super::cache::{RateLimit, RateLimitTracker, ResponseCache};
use super::error::RawResponse;
use super::requests::ApiRequest;

/// Maximum number of GitHub HTTP requests this backend may have in flight.
pub const MAX_CONCURRENT_REQUESTS: usize = 8;

#[derive(Debug)]
pub(crate) struct RequestLimiter {
    semaphore: tokio::sync::Semaphore,
}

impl Default for RequestLimiter {
    fn default() -> Self {
        Self {
            semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS),
        }
    }
}

impl RequestLimiter {
    pub(crate) async fn acquire(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::AcquireError> {
        self.semaphore.acquire().await
    }

    #[cfg(test)]
    fn try_acquire(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::TryAcquireError> {
        self.semaphore.try_acquire()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Owner {
    pub login: String,
    pub id: u64,
}

/// A repository capability reported by GitHub.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoPermission {
    Pull,
    Triage,
    Push,
    Maintain,
    Admin,
}

/// The signed-in user's repository capabilities, when GitHub includes them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoPermissions {
    #[serde(default)]
    pub pull: bool,
    #[serde(default)]
    pub triage: bool,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub maintain: bool,
    #[serde(default)]
    pub admin: bool,
}

impl RepoPermissions {
    /// Whether GitHub reports the requested capability for the signed-in user.
    pub fn allows(&self, permission: RepoPermission) -> bool {
        match permission {
            RepoPermission::Pull => self.pull,
            RepoPermission::Triage => self.triage,
            RepoPermission::Push => self.push,
            RepoPermission::Maintain => self.maintain,
            RepoPermission::Admin => self.admin,
        }
    }
}

/// The `author`/`committer` sub-object on a `/repos/{owner}/{repo}/commits`
/// entry — only present when the commit's git author/committer email is
/// linked to a GitHub account (`null` otherwise, e.g. commits from an email
/// with no matching account).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitAuthorInfo {
    pub login: String,
    pub avatar_url: String,
}

/// One entry from `/repos/{owner}/{repo}/commits` — just enough to map a
/// commit SHA to its GitHub author's avatar for the History tab; the actual
/// commit message/diff/etc. is read locally via `git log`, not this API.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitSummary {
    pub sha: String,
    pub author: Option<CommitAuthorInfo>,
    pub committer: Option<CommitAuthorInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repo {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub owner: Owner,
    pub private: bool,
    pub description: Option<String>,
    pub clone_url: String,
    #[serde(default)]
    pub html_url: String,
    pub visibility: String,
    pub archived: bool,
    pub pushed_at: String,
    pub has_issues: bool,
    pub has_wiki: bool,
    pub has_projects: bool,
    #[serde(default)]
    pub has_discussions: bool,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub ssh_url: String,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub stargazers_count: u64,
    #[serde(default)]
    pub forks_count: u64,
    #[serde(default)]
    pub open_issues_count: u64,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub permissions: Option<RepoPermissions>,
    /// Which ways of merging a pull request the repository allows. `None`
    /// when GitHub did not say, which is read as allowed.
    #[serde(default)]
    pub allow_merge_commit: Option<bool>,
    #[serde(default)]
    pub allow_squash_merge: Option<bool>,
    #[serde(default)]
    pub allow_rebase_merge: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    #[serde(default)]
    pub protected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoInvitation {
    pub id: u64,
    pub repository: RepoInvitationRepo,
    pub inviter: RepoInvitationInviter,
    pub permissions: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoInvitationRepo {
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub html_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoInvitationInviter {
    pub login: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    pub id: u64,
    pub name: Option<String>,
    pub bio: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub blog: Option<String>,
    #[serde(default)]
    pub twitter_username: Option<String>,
    pub avatar_url: String,
    pub public_repos: u64,
    pub total_private_repos: Option<u64>,
    pub followers: u64,
    pub following: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrgDetail {
    pub login: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub avatar_url: String,
    pub blog: Option<String>,
    pub location: Option<String>,
    pub email: Option<String>,
    pub twitter_username: Option<String>,
    #[serde(default)]
    pub is_verified: bool,
    pub public_repos: u64,
    #[serde(default)]
    pub total_private_repos: Option<u64>,
    pub followers: u64,
    pub html_url: String,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitHubUserDetail {
    pub login: String,
    pub id: u64,
    pub avatar_url: String,
    pub html_url: String,
    pub name: Option<String>,
    pub company: Option<String>,
    pub blog: Option<String>,
    pub location: Option<String>,
    pub email: Option<String>,
    pub bio: Option<String>,
    pub twitter_username: Option<String>,
    pub public_repos: u64,
    pub followers: u64,
    pub following: u64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(rename = "type")]
    pub user_type: String,
    #[serde(default)]
    pub hireable: Option<bool>,
    #[serde(default)]
    pub total_private_repos: Option<u64>,
}

/// The ref currently in view inside a Workspace tab: a repository plus the
/// branch/tag/commit it is reading from. This is the initial W1 backend model
/// for a single "what is the tab looking at" value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub ref_name: Option<String>,
    #[serde(default)]
    pub commit_sha: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRef {
    pub ref_name: String,
    pub commit_sha: String,
    pub tree_sha: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFile {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub changes: u64,
    pub patch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDetail {
    pub sha: String,
    pub message: String,
    pub files: Vec<CommitFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefMovement {
    Unchanged,
    Advanced { commits: u64 },
    Rewritten,
}

/// GitHub's comparison reports zero commits behind when `previous` is an
/// ancestor of `new`; a nonzero behind count means the branch was rewritten
/// or diverged.
pub fn classify_ref_movement(
    previous: &str,
    new: &str,
    ahead_by: u64,
    behind_by: u64,
) -> RefMovement {
    if previous == new {
        RefMovement::Unchanged
    } else if behind_by == 0 && ahead_by > 0 {
        RefMovement::Advanced { commits: ahead_by }
    } else {
        RefMovement::Rewritten
    }
}

/// One entry in a Git tree. The tree is the primary source for the code view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub kind: TreeEntryKind,
    #[serde(default)]
    pub sha: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeEntryKind {
    #[default]
    Blob,
    Tree,
    Commit,
    Symlink,
    Submodule,
}

/// A tree result as returned by the GitHub Trees endpoint. `truncated` means
/// the full tree did not fit in one response, so the caller may load a
/// directory or path at a time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoTree {
    pub sha: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub entries: Vec<TreeEntry>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    pub name: String,
    pub path: String,
    pub entry: TreeEntry,
    #[serde(default)]
    pub children: Vec<TreeNode>,
}

#[derive(Default)]
struct TreeBuilderNode {
    entry: Option<TreeEntry>,
    children: std::collections::BTreeMap<String, TreeBuilderNode>,
}

/// Turn GitHub's flat path list into sorted directory nodes. Missing tree
/// entries are synthesized so incomplete path lists still have a usable tree.
pub fn build_tree(entries: &[TreeEntry]) -> Vec<TreeNode> {
    let mut root = TreeBuilderNode::default();
    for entry in entries {
        let mut node = &mut root;
        for component in entry.path.split('/').filter(|part| !part.is_empty()) {
            node = node.children.entry(component.to_string()).or_default();
        }
        node.entry = Some(entry.clone());
    }
    root.children
        .into_iter()
        .map(|(name, node)| build_tree_node(name, String::new(), node))
        .collect()
}

fn build_tree_node(name: String, parent: String, node: TreeBuilderNode) -> TreeNode {
    let path = if parent.is_empty() {
        name.clone()
    } else {
        format!("{parent}/{name}")
    };
    let entry = node.entry.unwrap_or_else(|| TreeEntry {
        path: path.clone(),
        mode: "040000".to_string(),
        kind: TreeEntryKind::Tree,
        sha: String::new(),
        size: None,
        target: None,
    });
    let children = node
        .children
        .into_iter()
        .map(|(child_name, child)| build_tree_node(child_name, path.clone(), child))
        .collect();
    TreeNode {
        name,
        path,
        entry,
        children,
    }
}

/// A tree request can succeed without a tree when the repository has no
/// commits yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TreeLoadResult {
    #[default]
    EmptyRepository,
    Tree(RepoTree),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobKind {
    #[default]
    Text,
    Image,
    Binary,
    LfsPointer,
    TooLarge,
}

pub const MAX_INLINE_TEXT_BYTES: u64 = 1024 * 1024;
pub const MAX_INLINE_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileClassification {
    pub kind: BlobKind,
    pub size: u64,
    #[serde(default)]
    pub lfs_size: Option<u64>,
}

/// Classify a file from its path, known size, and available leading bytes.
pub fn classify_file(path: &str, size: u64, leading_bytes: &[u8]) -> FileClassification {
    const LFS_SIGNATURE: &[u8] = b"version https://git-lfs.github.com/spec/v1";
    if leading_bytes.starts_with(LFS_SIGNATURE) {
        let lfs_size = std::str::from_utf8(leading_bytes).ok().and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("size "))
                .and_then(|value| value.parse().ok())
        });
        return FileClassification {
            kind: BlobKind::LfsPointer,
            size,
            lfs_size,
        };
    }

    let image = is_image_path(path) || is_image_signature(leading_bytes);
    let too_large = if image {
        size > MAX_INLINE_IMAGE_BYTES
    } else {
        size > MAX_INLINE_TEXT_BYTES
    };
    let kind = if too_large {
        BlobKind::TooLarge
    } else if image {
        BlobKind::Image
    } else if leading_bytes.contains(&0)
        || std::str::from_utf8(leading_bytes).is_err_and(|error| error.error_len().is_some())
    {
        BlobKind::Binary
    } else {
        BlobKind::Text
    };
    FileClassification {
        kind,
        size,
        lfs_size: None,
    }
}

fn is_image_path(path: &str) -> bool {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "avif" | "bmp" | "gif" | "ico" | "jpeg" | "jpg" | "png" | "tif" | "tiff" | "webp"
    )
}

fn is_image_signature(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"BM")
        || (bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

/// A blob fetched by SHA, with its content kept as bytes until text is needed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobData {
    pub sha: String,
    #[serde(default)]
    pub kind: BlobKind,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub content: Vec<u8>,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub lfs_size: Option<u64>,
}

/// A README for a repo or a folder. The path is the GitHub path, not the
/// filesystem path in the app.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Readme {
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub encoding: String,
}

/// A single changed file in a compare result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompareFile {
    pub path: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    #[serde(default)]
    pub patch: Option<String>,
}

/// The comparison between two refs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompareResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub ahead_by: u64,
    #[serde(default)]
    pub behind_by: u64,
    #[serde(default)]
    pub commits: Vec<CommitSummary>,
    #[serde(default)]
    pub files: Vec<CompareFile>,
}

/// A search result item for repository search.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRepoResult {
    pub repo: Repo,
    #[serde(default)]
    pub score: Option<f64>,
}

/// One match in a code search result.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchCodeMatch {
    pub path: String,
    pub repo: String,
    #[serde(default)]
    pub sha: Option<String>,
    #[serde(default)]
    pub line: Option<String>,
    #[serde(default)]
    pub line_number: Option<u64>,
    #[serde(default)]
    pub matches: Vec<SearchCodeMatchFragment>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchCodeMatchFragment {
    pub text: String,
    #[serde(default)]
    pub start: Option<u64>,
    #[serde(default)]
    pub end: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collaborator {
    pub login: String,
    pub id: u64,
    pub avatar_url: String,
    pub role_name: String,
    pub permissions: Option<serde_json::Value>,
}

/// One member of an organisation, as its member list gives them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrgMember {
    pub login: String,
    pub id: u64,
    #[serde(default)]
    pub avatar_url: String,
}

/// Sub-object shared by issue/PR/release list items (`user`/`author`): just
/// enough login/avatar for a list row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GhListUser {
    pub login: String,
    #[serde(default)]
    pub avatar_url: String,
}

/// One label on an issue/PR.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GhListLabel {
    pub name: String,
}

/// One item from `/repos/{owner}/{repo}/issues`. Issues and PRs share that
/// endpoint — a non-null `pull_request` marks an item as a PR, which the
/// Issues tab filters out (the PRs tab uses `/pulls` directly instead).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub comments: u64,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub user: Option<GhListUser>,
    #[serde(default)]
    pub labels: Vec<GhListLabel>,
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

/// `head`/`base` branch info on a pull request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PullRef {
    #[serde(default)]
    pub label: String,
    #[serde(rename = "ref", default)]
    pub r#ref: String,
    #[serde(default)]
    pub sha: String,
}

/// One item from `/repos/{owner}/{repo}/pulls`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub title: String,
    pub state: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub user: Option<GhListUser>,
    #[serde(default)]
    pub head: PullRef,
    #[serde(default)]
    pub base: PullRef,
    #[serde(default)]
    pub draft: bool,
    /// Whether the pull request merges cleanly. Only a single pull request
    /// carries it, not the list, and GitHub works it out on request: `None`
    /// means it has not finished yet.
    #[serde(default)]
    pub mergeable: Option<bool>,
    /// GitHub's verdict on merging: `clean`, `unstable`, `blocked`, `behind`,
    /// `dirty`, `draft`, `has_hooks` or `unknown`. Single pull request only.
    #[serde(default)]
    pub mergeable_state: Option<String>,
}

/// One check run on a commit, from `/commits/{sha}/check-runs`: what GitHub
/// Actions and other checks report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckRun {
    #[serde(default)]
    pub id: u64,
    pub name: String,
    /// `queued`, `in_progress` or `completed`.
    #[serde(default)]
    pub status: String,
    /// Set once `status` is `completed`: `success`, `failure`, `neutral`,
    /// `cancelled`, `skipped`, `timed_out` or `action_required`.
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

/// GitHub's answer to merging a pull request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MergeResult {
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CombinedCommitStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub total_count: u64,
}

/// One comment on an issue or PR — `/repos/{owner}/{repo}/issues/{number}/comments`,
/// which (per GitHub's API) serves both: a PR's "conversation" comments live
/// on the same endpoint as an issue's, since every PR *is* an issue under
/// the hood. PR review comments (inline on a diff) are a separate endpoint
/// this doesn't cover.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub user: Option<GhListUser>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub html_url: String,
}

/// One downloadable file attached to a release.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub download_count: u64,
    #[serde(default)]
    pub browser_download_url: String,
}

/// One item from `/repos/{owner}/{repo}/releases`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub author: Option<GhListUser>,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// One row from `/users/{owner}/packages` or `/orgs/{owner}/packages`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    #[serde(rename = "package_type", default)]
    pub package_type: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub html_url: String,
}

/// One version of a package, from the package versions endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageVersion {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub created_at: String,
}

/// One day of view/clone counts from the traffic endpoints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrafficDay {
    pub timestamp: String,
    pub count: u64,
    pub uniques: u64,
}

/// `/repos/{owner}/{repo}/traffic/views`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrafficViews {
    pub count: u64,
    pub uniques: u64,
    #[serde(default)]
    pub views: Vec<TrafficDay>,
}

/// `/repos/{owner}/{repo}/traffic/clones`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrafficClones {
    pub count: u64,
    pub uniques: u64,
    #[serde(default)]
    pub clones: Vec<TrafficDay>,
}

/// `/repos/{owner}/{repo}/traffic/popular/referrers`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrafficReferrer {
    pub referrer: String,
    pub count: u64,
    pub uniques: u64,
}

/// `/repos/{owner}/{repo}/traffic/popular/paths`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrafficPath {
    pub path: String,
    #[serde(default)]
    pub title: String,
    pub count: u64,
    pub uniques: u64,
}

/// Aggregated traffic snapshot for the Traffic tab: views/clones are `None`
/// when the repo has no traffic data yet (GitHub returns 202/404 for those
/// endpoints); referrers/paths default to empty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepoTraffic {
    pub views: Option<TrafficViews>,
    pub clones: Option<TrafficClones>,
    pub referrers: Vec<TrafficReferrer>,
    pub paths: Vec<TrafficPath>,
}

/// One item from `/repos/{owner}/{repo}/deployments`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deployment {
    pub id: u64,
    #[serde(default)]
    pub environment: String,
    #[serde(rename = "ref", default)]
    pub r#ref: String,
    #[serde(default)]
    pub sha: String,
    #[serde(default)]
    pub created_at: String,
    /// Not present on the deployment object itself (GitHub only exposes
    /// status via the separate deployment-statuses endpoint) — defaults to
    /// "unknown" rather than making a second call per deployment.
    #[serde(default = "default_deployment_status")]
    pub status: String,
}

fn default_deployment_status() -> String {
    "unknown".to_string()
}

/// One item from `/repos/{owner}/{repo}/tags`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tag {
    pub name: String,
    #[serde(default)]
    pub commit: TagCommit,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagCommit {
    #[serde(default)]
    pub sha: String,
}

/// A pending (not yet accepted) org membership, from
/// `/user/memberships/orgs` filtered to `state == "pending"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrgInvitation {
    pub role: String,
    pub organization: OrgInvitationOrg,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrgInvitationOrg {
    pub login: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: u64,
    pub name: String,
    pub head_branch: Option<String>,
    pub head_sha: String,
    pub run_number: u64,
    pub event: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One step within a `WorkflowJob`, from the "List jobs for a workflow run"
/// endpoint's `steps[]` — this is the actual "which step is it on right
/// now" detail the plain run-list endpoint (`WorkflowRun` above) doesn't
/// carry at all.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub number: u64,
}

/// One job within a workflow run (`GET .../actions/runs/{id}/jobs`) — e.g.
/// `build_windows`/`build_linux`/`build_macos` for `build_installers.yml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowJob {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
}

/// Output of a running clone, or its end. Panels subscribe via
/// `GhState::clone_tx.subscribe()` while a clone is in flight. `Done`
/// carries no payload: the clone's result is returned by `clone_repo`
/// itself, and this event only marks "stop listening".
#[derive(Clone, Debug)]
pub enum CloneEvent {
    /// A finished line of output.
    Line(String),
    /// A progress update that replaces the previous one (git rewrites its
    /// "Receiving objects: 42%" line in place).
    Progress(String),
    Done,
}

/// What the API functions need: the host's HTTP client, the signed-in
/// account's token, the API base URL, and the channel clone reports progress
/// on. Shared via `Arc`.
pub struct GhState {
    /// The host application's HTTP client. Requests go through it so that
    /// they use the same proxy settings as the rest of the app.
    pub http: std::sync::Arc<dyn http_client::HttpClient>,
    /// The access token, handed in by the host with [`Self::set_token`].
    /// `None` until then, and after signing out.
    token: tokio::sync::RwLock<Option<String>>,
    /// The scopes GitHub last reported for the token (`X-OAuth-Scopes`).
    /// `None` until an answer has carried that header, and for tokens that
    /// do not report scopes, such as fine-grained personal access tokens.
    scopes: std::sync::Mutex<Option<Vec<String>>>,
    pub base_url: tokio::sync::RwLock<String>,
    pub clone_tx: tokio::sync::broadcast::Sender<CloneEvent>,
    /// Answers to earlier requests, by path, with their `ETag`s.
    cache: std::sync::Mutex<ResponseCache>,
    /// Per-resource rate-limit snapshots and secondary-limit cooldowns.
    rate: std::sync::Mutex<RateLimitTracker>,
    /// Limits in-flight API requests shared by every caller using this state.
    request_limiter: RequestLimiter,
}

impl GhState {
    pub fn new(http: std::sync::Arc<dyn http_client::HttpClient>) -> Self {
        let (clone_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            http,
            token: tokio::sync::RwLock::new(None),
            scopes: std::sync::Mutex::default(),
            base_url: tokio::sync::RwLock::new("https://api.github.com".to_string()),
            clone_tx,
            cache: std::sync::Mutex::default(),
            rate: std::sync::Mutex::default(),
            request_limiter: RequestLimiter::default(),
        }
    }

    /// Uses `token` for every request from now on, or no token at all.
    /// Answers, scopes and rate limits remembered under the previous token
    /// are forgotten, since they belong to that sign-in.
    pub async fn set_token(&self, token: Option<String>) {
        *self.token.write().await = token;
        self.forget_answers();
        self.forget_rate_limits();
        *self.scopes_lock() = None;
    }

    /// The token requests are sent with, if one has been set.
    pub async fn token(&self) -> Option<String> {
        self.token.read().await.clone()
    }

    /// The scopes GitHub last reported for the token. `None` when no answer
    /// has reported them yet, or the token kind does not report scopes.
    pub fn token_scopes(&self) -> Option<Vec<String>> {
        self.scopes_lock().clone()
    }

    pub(crate) fn record_token_scopes(&self, scopes: Vec<String>) {
        *self.scopes_lock() = Some(scopes);
    }

    fn scopes_lock(&self) -> std::sync::MutexGuard<'_, Option<Vec<String>>> {
        self.scopes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The remembered answers. A lock poisoned by a panic elsewhere is
    /// still usable: no half-finished update can leave the cache wrong in
    /// a way that matters.
    pub(crate) fn cache(&self) -> std::sync::MutexGuard<'_, ResponseCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The answer last given to `request`, if it is remembered, as a
    /// successful response. Nothing is sent.
    pub(crate) fn remembered(&self, request: &ApiRequest) -> Option<RawResponse> {
        if request.method != "GET" {
            return None;
        }
        let cached = self.cache().get(&request.path, request.accept)?;
        Some(RawResponse {
            status: 200,
            body: String::from_utf8_lossy(&cached.body).into_owned(),
            body_bytes: cached.body,
            link: cached.link,
            ..RawResponse::default()
        })
    }

    /// Forgets every remembered answer. Done automatically after a change
    /// is sent to GitHub and when the sign-in changes.
    pub fn forget_answers(&self) {
        self.cache().clear();
    }

    /// How much of the rate limit is left, as of the last answer. `None`
    /// until a request has been answered.
    pub fn rate_limit(&self) -> Option<RateLimit> {
        self.rate_limit_for("core")
    }

    /// The latest rate-limit snapshot for a GitHub API resource.
    pub fn rate_limit_for(&self, resource: &str) -> Option<RateLimit> {
        self.rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(resource)
    }

    pub(crate) fn rate_limit_retry_at(&self, resource: &str) -> Option<u64> {
        self.rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retry_at(resource)
    }

    pub(crate) fn record_rate_limit(
        &self,
        resource: &str,
        rate: Option<RateLimit>,
        retry_after: Option<u64>,
        now: u64,
    ) {
        self.rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(resource, rate, retry_after, now);
    }

    pub(crate) fn forget_rate_limits(&self) {
        self.rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub(crate) async fn acquire_request_slot(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::AcquireError> {
        self.request_limiter.acquire().await
    }
}

#[cfg(test)]
mod workspace_file_tests {
    use super::*;

    #[test]
    fn repository_permissions_are_typed_and_answer_capability_checks() {
        let permissions: RepoPermissions = serde_json::from_value(serde_json::json!({
            "pull": true,
            "triage": false,
            "push": true,
            "maintain": false,
            "admin": false
        }))
        .unwrap();
        assert!(permissions.allows(RepoPermission::Pull));
        assert!(!permissions.allows(RepoPermission::Triage));
        assert!(permissions.allows(RepoPermission::Push));
        assert!(!permissions.allows(RepoPermission::Maintain));
        assert!(!permissions.allows(RepoPermission::Admin));

        let defaulted: RepoPermissions = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(defaulted, RepoPermissions::default());

        let serialized = serde_json::to_value(RepoPermission::Maintain).unwrap();
        assert_eq!(serialized, "maintain");
    }

    #[test]
    fn repository_permissions_can_be_absent_or_deserialized_from_repository_response() {
        let mut response = serde_json::json!({
            "id": 1,
            "name": "project",
            "full_name": "owner/project",
            "owner": { "login": "owner", "id": 2 },
            "private": false,
            "description": null,
            "clone_url": "",
            "visibility": "public",
            "archived": false,
            "pushed_at": "",
            "has_issues": true,
            "has_wiki": true,
            "has_projects": true,
            "permissions": { "pull": true, "push": false }
        });
        let repo: Repo = serde_json::from_value(response.clone()).unwrap();
        let permissions = repo.permissions.unwrap();
        assert!(permissions.allows(RepoPermission::Pull));
        assert!(!permissions.allows(RepoPermission::Push));
        assert!(!permissions.allows(RepoPermission::Admin));

        response.as_object_mut().unwrap().remove("permissions");
        let repo: Repo = serde_json::from_value(response).unwrap();
        assert!(repo.permissions.is_none());
    }

    fn entry(path: &str, mode: &str, kind: TreeEntryKind) -> TreeEntry {
        TreeEntry {
            path: path.into(),
            mode: mode.into(),
            kind,
            sha: format!("sha-{path}"),
            size: None,
            target: None,
        }
    }

    #[test]
    fn flat_tree_paths_build_sorted_nested_nodes() {
        let entries = vec![
            entry("src/main.rs", "100644", TreeEntryKind::Blob),
            entry("README.md", "100644", TreeEntryKind::Blob),
            entry("src/lib.rs", "100644", TreeEntryKind::Blob),
            entry("vendor", "160000", TreeEntryKind::Submodule),
        ];
        let tree = build_tree(&entries);
        assert_eq!(
            tree.iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            vec!["README.md", "src", "vendor"]
        );
        assert_eq!(tree[1].entry.kind, TreeEntryKind::Tree);
        assert_eq!(
            tree[1]
                .children
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            vec!["lib.rs", "main.rs"]
        );
        assert_eq!(tree[2].entry.kind, TreeEntryKind::Submodule);
    }

    #[test]
    fn files_are_classified_by_limits_images_lfs_and_content() {
        assert_eq!(
            classify_file("src/main.rs", 4, b"fn x"),
            FileClassification {
                kind: BlobKind::Text,
                size: 4,
                lfs_size: None,
            }
        );
        assert_eq!(
            classify_file("assets/logo.png", 20, b"not-decoded-yet"),
            FileClassification {
                kind: BlobKind::Image,
                size: 20,
                lfs_size: None,
            }
        );
        assert_eq!(
            classify_file("opaque.dat", 4, &[0, 255, 128, 10]).kind,
            BlobKind::Binary
        );
        assert_eq!(classify_file("text.txt", 2, &[0xC3]).kind, BlobKind::Text);
        assert_eq!(
            classify_file("large.txt", MAX_INLINE_TEXT_BYTES + 1, b"text").kind,
            BlobKind::TooLarge
        );
        assert_eq!(
            classify_file(
                "large.png",
                MAX_INLINE_IMAGE_BYTES + 1,
                b"\x89PNG\r\n\x1a\n"
            )
            .kind,
            BlobKind::TooLarge
        );
        let pointer =
            b"version https://git-lfs.github.com/spec/v1\noid sha256:deadbeef\nsize 123456\n";
        assert_eq!(
            classify_file("assets/model.bin", pointer.len() as u64, pointer),
            FileClassification {
                kind: BlobKind::LfsPointer,
                size: pointer.len() as u64,
                lfs_size: Some(123456),
            }
        );
    }
}

#[cfg(test)]
mod request_limiter_tests {
    use super::*;

    #[test]
    fn request_limiter_caps_shared_in_flight_requests() {
        let limiter = RequestLimiter::default();
        let permits: Vec<_> = (0..MAX_CONCURRENT_REQUESTS)
            .map(|_| limiter.try_acquire().expect("slot within configured cap"))
            .collect();

        assert!(limiter.try_acquire().is_err());
        drop(permits);
        assert!(limiter.try_acquire().is_ok());
    }
}
