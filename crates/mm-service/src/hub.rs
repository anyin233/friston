//! Downloading model checkpoints from the Hugging Face Hub (or a mirror such as `hf-mirror.com`).
//!
//! The engines load models from plain folders, so a download is a snapshot of every file of
//! one repo revision written into `<data dir>/models/<owner>--<name>/`. Files already on disk
//! with the expected size are skipped and partial `.part` files resume with a `Range`
//! request, so a 1.5 GB checkpoint survives a dropped connection or a cancel.
//!
//! The access token (needed for gated or private repos) lives in the macOS Keychain under
//! service `Friston`, account `huggingface`; `HF_TOKEN` is the fallback so tokens
//! set up for `huggingface_hub` keep working.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mm_i18n::{tr, trf};
use mm_llm::keychain::{SystemKeychain, read_secret, write_secret};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

/// Repo the settings page suggests: the MLX conversion that `mm-embedd` loads.
pub const DEFAULT_EMBEDDING_REPO_URL: &str =
    "https://huggingface.co/mlx-community/embeddinggemma-2-bf16";

/// Hub used when the user gives a bare `owner/name` and `HF_ENDPOINT` is not set.
const DEFAULT_ENDPOINT: &str = "https://huggingface.co";

/// Keychain account holding the Hugging Face token (service: `mm_llm::keychain::KEYCHAIN_SERVICE`).
const KEYCHAIN_ACCOUNT: &str = "huggingface";

/// Repo files that are Git plumbing, not part of the checkpoint.
const SKIPPED_FILES: &[&str] = &[".gitattributes"];

/// Written into a snapshot folder after its last file landed, so a later launch can tell a
/// finished download from an interrupted one without asking the Hub.
const COMPLETE_MARKER: &str = ".mm-download-complete";

/// Bytes between progress callbacks, so the UI is not flooded on fast links.
const PROGRESS_STEP: u64 = 1 << 20;

/// Characters escaped inside one URL path segment (unreserved characters stay as they are).
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Why a download stopped. `Display` follows the UI language (see the impl below).
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    BadUrl(String),
    Unauthorized {
        repo: String,
        status: u16,
    },
    NotFound(String),
    Http {
        url: String,
        message: String,
    },
    Io {
        path: PathBuf,
        message: String,
    },
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    UnsafePath(String),
    Keychain(String),
    Cancelled,
}

impl std::fmt::Display for HubError {
    /// Writes the message in the current UI language; written by hand because
    /// `#[error("…")]` cannot pick a language at runtime.
    ///
    /// Params: `f` – formatter.
    /// Returns: the formatter's result.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            HubError::BadUrl(u) => trf!(
                "无法识别的模型地址“{u}”，应形如 https://huggingface.co/作者/模型 或 作者/模型",
                "Unrecognized model address “{u}”. Use https://huggingface.co/owner/model or owner/model"
            ),
            HubError::Unauthorized { repo, status } => trf!(
                "没有权限下载 {repo}（HTTP {status}）：请在设置里填写 Hugging Face 令牌，并确认已在网页上同意该模型的使用条款",
                "No permission to download {repo} (HTTP {status}). Add a Hugging Face token in Settings and accept the model's terms on its web page"
            ),
            HubError::NotFound(r) => trf!(
                "找不到模型 {r}：请检查地址和版本",
                "Model {r} not found. Check the address and revision"
            ),
            HubError::Http { url, message } => {
                trf!(
                    "下载 {url} 失败：{message}",
                    "Couldn't download {url}: {message}"
                )
            }
            HubError::Io { path, message } => trf!(
                "写入 {} 失败：{message}",
                "Couldn't write {}: {message}",
                path.display()
            ),
            HubError::SizeMismatch {
                path,
                expected,
                actual,
            } => trf!(
                "{} 大小不对：应为 {expected} 字节，实际 {actual} 字节",
                "{} has the wrong size: expected {expected} bytes, got {actual} bytes",
                path.display()
            ),
            HubError::UnsafePath(p) => {
                trf!("模型文件名不安全：{p}", "Unsafe model file name: {p}")
            }
            HubError::Keychain(m) => trf!("钥匙串：{m}", "Keychain: {m}"),
            HubError::Cancelled => tr!("下载已取消", "Download cancelled").to_string(),
        };
        f.write_str(&text)
    }
}

/// One repo at one revision on one Hub endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubRepo {
    /// `https://huggingface.co` or a mirror, without a trailing slash.
    pub endpoint: String,
    /// `owner/name`.
    pub id: String,
    /// Branch, tag or commit; `main` unless the URL says `/tree/<rev>`.
    pub revision: String,
}

impl HubRepo {
    /// Parses what a user pastes: a model page URL (optionally `/tree/<rev>`), or `owner/name`.
    ///
    /// Params: `input` – the text; surrounding whitespace and a trailing slash are ignored.
    /// Returns: the repo, with the endpoint taken from the URL (so mirrors work) or from
    /// `HF_ENDPOINT` / the default Hub for a bare id.
    pub fn parse(input: &str) -> Result<Self, HubError> {
        let bad = || HubError::BadUrl(input.trim().to_string());
        let text = input.trim().trim_end_matches('/');
        let (endpoint, path) = match text.split_once("://") {
            Some((scheme, rest)) => {
                if scheme != "https" && scheme != "http" {
                    return Err(bad());
                }
                let (host, path) = rest.split_once('/').ok_or_else(bad)?;
                if host.is_empty() {
                    return Err(bad());
                }
                (format!("{scheme}://{host}"), path.to_string())
            }
            None => {
                let endpoint = std::env::var("HF_ENDPOINT")
                    .ok()
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
                (endpoint.trim_end_matches('/').to_string(), text.to_string())
            }
        };
        // Model URLs may carry a `models/` prefix; datasets and spaces are not checkpoints.
        let path = path.strip_prefix("models/").unwrap_or(&path);
        let mut parts = path.split('/');
        let (owner, name) = match (parts.next(), parts.next()) {
            (Some(o), Some(n)) if !o.is_empty() && !n.is_empty() => (o, n),
            _ => return Err(bad()),
        };
        if matches!(owner, "datasets" | "spaces") || owner.starts_with('.') || name.starts_with('.')
        {
            return Err(bad());
        }
        let rest: Vec<&str> = parts.collect();
        let revision = match rest.as_slice() {
            [] => "main".to_string(),
            // `/tree/<rev>` and `/blob/<rev>/…` both name a revision; revisions may hold `/`
            // (`refs/pr/1`), so for `tree` everything after it is the revision.
            ["tree", rev @ ..] if !rev.is_empty() => rev.join("/"),
            _ => return Err(bad()),
        };
        Ok(Self {
            endpoint,
            id: format!("{owner}/{name}"),
            revision,
        })
    }

    /// Folder name for this repo under the models folder (`owner--name`, like the Hub cache).
    ///
    /// Returns: the folder name.
    pub fn folder_name(&self) -> String {
        self.id.replace('/', "--")
    }

    /// The model page, shown back to the user.
    ///
    /// Returns: e.g. `https://huggingface.co/mlx-community/embeddinggemma-2-bf16`.
    pub fn page_url(&self) -> String {
        format!("{}/{}", self.endpoint, self.id)
    }
}

/// A file of the repo snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    /// Path inside the repo, `/`-separated.
    pub path: String,
    /// Size in bytes, when the Hub reported it.
    pub size: Option<u64>,
}

/// The file list of a repo at a resolved commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListing {
    /// Commit the revision resolved to; files are fetched at this commit so a push during
    /// the download cannot mix two versions.
    pub sha: String,
    pub files: Vec<RepoFile>,
}

/// Progress of a running download.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Progress {
    /// File being fetched.
    pub file: String,
    /// Bytes on disk across all files (including skipped and resumed ones).
    pub done_bytes: u64,
    /// Total bytes of the snapshot (files without a reported size count as 0).
    pub total_bytes: u64,
    /// Files finished.
    pub files_done: usize,
    pub files_total: usize,
}

/// `GET /api/models/<id>/revision/<rev>?blobs=true` response (only the fields we use).
#[derive(Deserialize)]
struct ApiModel {
    sha: String,
    #[serde(default)]
    siblings: Vec<ApiSibling>,
}

/// One entry of `siblings`.
#[derive(Deserialize)]
struct ApiSibling {
    rfilename: String,
    size: Option<u64>,
}

/// Builds the HTTP agent used for the Hub.
///
/// Returns: an agent that reports HTTP errors as statuses and keeps the token across the
/// Hub's same-host redirects (`/resolve` → `/api/resolve-cache`); signed CDN redirects to
/// other hosts never see the token.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .into()
}

/// Sends a GET with an optional bearer token and maps auth / not-found statuses.
///
/// Params: `agent`; `repo` – for error messages; `url`; `token`; `range_from` – resume offset.
/// Returns: the response (status 200 or 206).
fn get(
    agent: &ureq::Agent,
    repo: &HubRepo,
    url: &str,
    token: Option<&str>,
    range_from: u64,
) -> Result<ureq::http::Response<ureq::Body>, HubError> {
    let mut req = agent.get(url);
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    if range_from > 0 {
        req = req.header("Range", format!("bytes={range_from}-"));
    }
    let resp = req.call().map_err(|e| HubError::Http {
        url: url.to_string(),
        message: e.to_string(),
    })?;
    match resp.status().as_u16() {
        200 | 206 => Ok(resp),
        s @ (401 | 403) => Err(HubError::Unauthorized {
            repo: repo.id.clone(),
            status: s,
        }),
        404 => Err(HubError::NotFound(format!("{}@{}", repo.id, repo.revision))),
        s => Err(HubError::Http {
            url: url.to_string(),
            message: format!("HTTP {s}"),
        }),
    }
}

/// Lists a repo's files at a revision.
///
/// Params: `repo`; `token` – bearer token for gated/private repos.
/// Returns: the resolved commit and its files (Git plumbing such as `.gitattributes` removed).
pub fn list_files(repo: &HubRepo, token: Option<&str>) -> Result<RepoListing, HubError> {
    list_files_with(&agent(), repo, token)
}

/// [`list_files`] with a given agent.
fn list_files_with(
    agent: &ureq::Agent,
    repo: &HubRepo,
    token: Option<&str>,
) -> Result<RepoListing, HubError> {
    let url = format!(
        "{}/api/models/{}/revision/{}?blobs=true",
        repo.endpoint,
        repo.id,
        utf8_percent_encode(&repo.revision, SEGMENT)
    );
    let mut resp = get(agent, repo, &url, token, 0)?;
    let model: ApiModel = resp.body_mut().read_json().map_err(|e| HubError::Http {
        url: url.clone(),
        message: trf!("解析文件列表：{e}", "parsing the file list: {e}"),
    })?;
    let files = model
        .siblings
        .into_iter()
        .filter(|s| !SKIPPED_FILES.contains(&s.rfilename.as_str()))
        .map(|s| RepoFile {
            path: s.rfilename,
            size: s.size,
        })
        .collect();
    Ok(RepoListing {
        sha: model.sha,
        files,
    })
}

/// Maps a repo path to a path under `dest`, refusing anything that could escape it.
///
/// Params: `dest` – snapshot folder; `rel` – `/`-separated repo path.
/// Returns: the local path.
fn local_path(dest: &Path, rel: &str) -> Result<PathBuf, HubError> {
    let unsafe_path = || HubError::UnsafePath(rel.to_string());
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
        return Err(unsafe_path());
    }
    let mut out = dest.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(unsafe_path());
        }
        out.push(seg);
    }
    Ok(out)
}

/// Size of a file, or 0 when it does not exist.
///
/// Params: `p`.
/// Returns: the length in bytes.
fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Tracks progress and throttles callbacks.
struct Reporter<'a> {
    progress: Progress,
    last_sent: u64,
    on_progress: &'a mut dyn FnMut(&Progress),
}

impl Reporter<'_> {
    /// Adds bytes and reports when a step has passed (or `force`).
    ///
    /// Params: `bytes` – newly written; `force` – report regardless of the step.
    fn add(&mut self, bytes: u64, force: bool) {
        self.progress.done_bytes += bytes;
        if force || self.progress.done_bytes - self.last_sent >= PROGRESS_STEP {
            self.last_sent = self.progress.done_bytes;
            (self.on_progress)(&self.progress);
        }
    }
}

/// Downloads one file into place (via `<name>.part`, resumed if present).
///
/// Params: `agent`; `repo`; `sha` – commit to fetch from; `file`; `target` – final path;
/// `token`; `cancel`; `rep` – progress.
/// Returns: `Ok(())` once the complete file was renamed into place.
#[allow(clippy::too_many_arguments)]
fn fetch_file(
    agent: &ureq::Agent,
    repo: &HubRepo,
    sha: &str,
    file: &RepoFile,
    target: &Path,
    token: Option<&str>,
    cancel: &AtomicBool,
    rep: &mut Reporter<'_>,
) -> Result<(), HubError> {
    let io = |path: &Path, e: std::io::Error| HubError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    let part = PathBuf::from(format!("{}.part", target.display()));
    let mut offset = file_len(&part);
    if file.size.is_some_and(|s| offset > s) {
        // A stale partial from a different upload; start over.
        std::fs::remove_file(&part).map_err(|e| io(&part, e))?;
        offset = 0;
    }
    let encoded: Vec<String> = file
        .path
        .split('/')
        .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
        .collect();
    let url = format!(
        "{}/{}/resolve/{sha}/{}",
        repo.endpoint,
        repo.id,
        encoded.join("/")
    );
    let resp = if file.size == Some(offset) && offset > 0 {
        None
    } else {
        Some(get(agent, repo, &url, token, offset)?)
    };
    if let Some(resp) = resp {
        let resumed = resp.status().as_u16() == 206;
        if !resumed && offset > 0 {
            // The server ignored the range: what we already counted is rewritten from 0.
            rep.progress.done_bytes -= offset;
            offset = 0;
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(resumed)
            .truncate(!resumed)
            .open(&part)
            .map_err(|e| io(&part, e))?;
        let mut body = resp.into_body().into_reader();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(HubError::Cancelled);
            }
            let n = body.read(&mut buf).map_err(|e| HubError::Http {
                url: url.clone(),
                message: e.to_string(),
            })?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n]).map_err(|e| io(&part, e))?;
            offset += n as u64;
            rep.add(n as u64, false);
        }
        out.flush().map_err(|e| io(&part, e))?;
    }
    if let Some(expected) = file.size
        && offset != expected
    {
        return Err(HubError::SizeMismatch {
            path: part,
            expected,
            actual: offset,
        });
    }
    std::fs::rename(&part, target).map_err(|e| io(target, e))
}

/// Downloads every file of `repo` into `models_root/<owner>--<name>/`.
///
/// Params: `repo`; `models_root` – folder holding all downloaded models; `token`;
/// `cancel` – set from another thread to stop between chunks (partials are kept for resume);
/// `on_progress` – called with throttled progress, and once per finished file.
/// Returns: the snapshot folder, ready to load.
pub fn download(
    repo: &HubRepo,
    models_root: &Path,
    token: Option<&str>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&Progress),
) -> Result<PathBuf, HubError> {
    let agent = agent();
    let listing = list_files_with(&agent, repo, token)?;
    let dest = models_root.join(repo.folder_name());
    // Validate every path before writing anything, so a hostile listing writes nothing.
    let targets = listing
        .files
        .iter()
        .map(|f| local_path(&dest, &f.path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut rep = Reporter {
        progress: Progress {
            total_bytes: listing.files.iter().filter_map(|f| f.size).sum(),
            files_total: listing.files.len(),
            ..Progress::default()
        },
        last_sent: 0,
        on_progress,
    };
    for (file, target) in listing.files.iter().zip(&targets) {
        if cancel.load(Ordering::Relaxed) {
            return Err(HubError::Cancelled);
        }
        rep.progress.file = file.path.clone();
        let have = file_len(target);
        if target.is_file() && file.size.is_some_and(|s| s == have) {
            rep.progress.files_done += 1;
            rep.add(have, true);
            continue;
        }
        // Bytes already in a partial count as done so the bar does not jump backwards.
        let part = PathBuf::from(format!("{}.part", target.display()));
        let partial = file_len(&part).min(file.size.unwrap_or(u64::MAX));
        rep.add(partial, true);
        fetch_file(
            &agent,
            repo,
            &listing.sha,
            file,
            target,
            token,
            cancel,
            &mut rep,
        )?;
        rep.progress.files_done += 1;
        rep.add(0, true);
    }
    let marker = dest.join(COMPLETE_MARKER);
    std::fs::write(&marker, format!("{}@{}\n", repo.id, listing.sha)).map_err(|e| {
        HubError::Io {
            path: marker.clone(),
            message: e.to_string(),
        }
    })?;
    tracing::info!(
        "downloaded {}@{} ({}) to {}",
        repo.id,
        repo.revision,
        listing.sha,
        dest.display()
    );
    Ok(dest)
}

/// The `huggingface_hub` cache folder, following its environment variables.
///
/// Returns: `HF_HUB_CACHE`, else `HF_HOME/hub`, else `~/.cache/huggingface/hub`; `None`
/// only when no home folder is known.
pub fn hf_cache_dir() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    env("HF_HUB_CACHE")
        .or_else(|| env("HF_HOME").map(|h| h.join("hub")))
        .or_else(|| dirs::home_dir().map(|h| h.join(".cache/huggingface/hub")))
}

/// Whether `dir` looks like a loadable checkpoint: a `config.json` and at least one
/// `.safetensors` file, following symlinks (the Hub cache links snapshots to blobs).
///
/// Params: `dir` – candidate folder.
/// Returns: `true` when both are present.
fn has_checkpoint(dir: &Path) -> bool {
    dir.join("config.json").is_file()
        && std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|e| {
                let p = e.path();
                p.extension().is_some_and(|x| x == "safetensors") && p.is_file()
            })
        })
}

/// Finds an already-downloaded copy of `repo`, so the default model loads without network.
///
/// Looks first in the app's own models folder (only finished downloads count), then in the
/// `huggingface_hub` cache, where `revision` resolves through `refs/<revision>` or names a
/// snapshot commit directly.
/// Params: `repo`; `models_root` – the app's models folder; `hf_cache` – the Hub cache
/// folder, usually [`hf_cache_dir`].
/// Returns: the snapshot folder, or `None` when the model still has to be downloaded.
pub fn find_local_snapshot(
    repo: &HubRepo,
    models_root: &Path,
    hf_cache: Option<&Path>,
) -> Option<PathBuf> {
    let own = models_root.join(repo.folder_name());
    if own.join(COMPLETE_MARKER).is_file() && has_checkpoint(&own) {
        return Some(own);
    }
    let entry = hf_cache?.join(format!("models--{}", repo.folder_name()));
    let commit = std::fs::read_to_string(entry.join("refs").join(&repo.revision))
        .map(|c| c.trim().to_string())
        .unwrap_or_else(|_| repo.revision.clone());
    // A ref name must not walk out of `snapshots/` (`refs/pr/1` has no snapshot of that name).
    if commit.is_empty() || commit.contains('/') || commit.starts_with('.') {
        return None;
    }
    let snap = entry.join("snapshots").join(commit);
    has_checkpoint(&snap).then_some(snap)
}

/// Reads the Hugging Face token stored in the Keychain.
///
/// Returns: the token, or `None` when none is stored.
pub fn read_stored_token() -> Result<Option<String>, HubError> {
    read_secret(&SystemKeychain, KEYCHAIN_ACCOUNT)
        .map_err(|e| HubError::Keychain(format!("reading {KEYCHAIN_ACCOUNT}: {e}")))
}

/// Stores (or with `None`, deletes) the Hugging Face token in the Keychain.
///
/// Params: `token`.
/// Returns: `Ok(())` once the Keychain accepted the change.
pub fn write_stored_token(token: Option<&str>) -> Result<(), HubError> {
    write_secret(&SystemKeychain, KEYCHAIN_ACCOUNT, token)
        .map_err(|e| HubError::Keychain(format!("writing {KEYCHAIN_ACCOUNT}: {e}")))
}

/// Token for downloads: the Keychain entry, else `HF_TOKEN`.
///
/// Returns: the token, or `None` for anonymous downloads.
pub fn resolve_token() -> Result<Option<String>, HubError> {
    if let Some(t) = read_stored_token()?.filter(|t| !t.trim().is_empty()) {
        return Ok(Some(t.trim().to_string()));
    }
    Ok(std::env::var("HF_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead as _, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[test]
    fn parses_urls_ids_and_mirrors() {
        let r = HubRepo::parse(" https://huggingface.co/mlx-community/embeddinggemma-2-bf16/ ")
            .unwrap();
        assert_eq!(r.endpoint, "https://huggingface.co");
        assert_eq!(r.id, "mlx-community/embeddinggemma-2-bf16");
        assert_eq!(r.revision, "main");
        assert_eq!(r.folder_name(), "mlx-community--embeddinggemma-2-bf16");
        assert_eq!(r.page_url(), DEFAULT_EMBEDDING_REPO_URL);

        let m =
            HubRepo::parse("https://hf-mirror.com/google/embeddinggemma-2/tree/refs/pr/3").unwrap();
        assert_eq!(m.endpoint, "https://hf-mirror.com");
        assert_eq!(m.revision, "refs/pr/3");

        let bare = HubRepo::parse("google/embeddinggemma-2").unwrap();
        assert_eq!(bare.id, "google/embeddinggemma-2");

        for bad in [
            "",
            "embeddinggemma",
            "https://huggingface.co/",
            "https://huggingface.co/datasets/x/y",
            "ftp://huggingface.co/a/b",
            "https://huggingface.co/a/b/blob/main/config.json",
            "../x",
        ] {
            assert!(
                matches!(HubRepo::parse(bad), Err(HubError::BadUrl(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn local_paths_stay_inside_the_snapshot() {
        let d = Path::new("/m");
        assert_eq!(
            local_path(d, "1_Pooling/config.json").unwrap(),
            Path::new("/m/1_Pooling/config.json")
        );
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "a\\b",
            "./a",
        ] {
            assert!(local_path(d, bad).is_err(), "{bad}");
        }
    }

    /// Requests the fake Hub saw: (path, authorization header, range header).
    type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

    /// Starts a tiny HTTP/1.1 server answering from `routes` (path → status, body); `Range`
    /// requests on 200 routes get 206 with the tail. Returns its base URL and the request log.
    fn fake_hub(routes: HashMap<String, (u16, Vec<u8>)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let (mut auth, mut range) = (None, None);
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(": ").unwrap();
                    match k.to_ascii_lowercase().as_str() {
                        "authorization" => auth = Some(v.to_string()),
                        "range" => range = Some(v.to_string()),
                        _ => {}
                    }
                }
                log.lock()
                    .unwrap()
                    .push((path.clone(), auth, range.clone()));
                let (mut status, mut body) = routes.get(&path).cloned().unwrap_or((404, vec![]));
                if status == 200
                    && let Some(from) = range
                        .as_deref()
                        .and_then(|r| r.strip_prefix("bytes="))
                        .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
                {
                    status = 206;
                    body = body[from..].to_vec();
                }
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (base, seen)
    }

    /// Listing JSON with the given (name, size) siblings.
    fn listing(files: &[(&str, usize)]) -> Vec<u8> {
        let siblings: Vec<_> = files
            .iter()
            .map(|(n, s)| serde_json::json!({"rfilename": n, "size": s}))
            .collect();
        serde_json::json!({"sha": "abc123", "siblings": siblings})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn downloads_resumes_and_skips_complete_files() {
        let weights = vec![7u8; 3 * 1024 * 1024 + 5];
        let routes = HashMap::from([
            (
                "/api/models/o/m/revision/main?blobs=true".to_string(),
                (
                    200,
                    listing(&[
                        (".gitattributes", 3),
                        ("config.json", 2),
                        ("1_Pooling/config.json", 2),
                        ("model.safetensors", weights.len()),
                    ]),
                ),
            ),
            (
                "/o/m/resolve/abc123/config.json".into(),
                (200, b"{}".to_vec()),
            ),
            (
                "/o/m/resolve/abc123/1_Pooling/config.json".into(),
                (200, b"[]".to_vec()),
            ),
            (
                "/o/m/resolve/abc123/model.safetensors".into(),
                (200, weights.clone()),
            ),
        ]);
        let (base, seen) = fake_hub(routes);
        let repo = HubRepo::parse(&format!("{base}/o/m")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let snap = root.path().join("o--m");
        // A partial left by an earlier, interrupted run.
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("config.json"), b"{}").unwrap();
        std::fs::write(snap.join("model.safetensors.part"), &weights[..1000]).unwrap();
        assert_eq!(
            find_local_snapshot(&repo, root.path(), None),
            None,
            "an interrupted download is not loadable"
        );

        let mut last = Progress::default();
        let dest = download(
            &repo,
            root.path(),
            Some("hf_test"),
            &AtomicBool::new(false),
            &mut |p| last = p.clone(),
        )
        .unwrap();
        assert_eq!(dest, snap);
        assert_eq!(
            std::fs::read(dest.join("model.safetensors")).unwrap(),
            weights
        );
        assert_eq!(
            std::fs::read(dest.join("1_Pooling/config.json")).unwrap(),
            b"[]"
        );
        assert!(!dest.join(".gitattributes").exists());
        assert!(!dest.join("model.safetensors.part").exists());
        assert_eq!(last.files_done, 3);
        assert_eq!(last.done_bytes, last.total_bytes);

        let log = seen.lock().unwrap().clone();
        assert!(
            log.iter()
                .all(|(_, a, _)| a.as_deref() == Some("Bearer hf_test")),
            "{log:?}"
        );
        let weights_req = log
            .iter()
            .find(|(p, _, _)| p.ends_with("model.safetensors"))
            .unwrap();
        assert_eq!(weights_req.2.as_deref(), Some("bytes=1000-"));

        // A second run finds everything in place and fetches only the listing.
        seen.lock().unwrap().clear();
        download(
            &repo,
            root.path(),
            None,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(find_local_snapshot(&repo, root.path(), None), Some(snap));
    }

    #[test]
    fn finds_snapshots_in_the_hugging_face_cache() {
        let repo = HubRepo::parse("mlx-community/embeddinggemma-2-bf16").unwrap();
        let models = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let entry = cache
            .path()
            .join("models--mlx-community--embeddinggemma-2-bf16");
        let snap = entry.join("snapshots/abc123");
        assert_eq!(
            find_local_snapshot(&repo, models.path(), Some(cache.path())),
            None
        );

        // huggingface_hub links finished blobs into the snapshot; a missing weight link
        // means the download never finished.
        std::fs::create_dir_all(entry.join("blobs")).unwrap();
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::create_dir_all(entry.join("refs")).unwrap();
        std::fs::write(entry.join("refs/main"), "abc123\n").unwrap();
        std::fs::write(entry.join("blobs/cfg"), b"{}").unwrap();
        std::os::unix::fs::symlink("../../blobs/cfg", snap.join("config.json")).unwrap();
        assert_eq!(
            find_local_snapshot(&repo, models.path(), Some(cache.path())),
            None
        );
        std::os::unix::fs::symlink("../../blobs/w", snap.join("model.safetensors")).unwrap();
        assert_eq!(
            find_local_snapshot(&repo, models.path(), Some(cache.path())),
            None,
            "a dangling weight link is not a checkpoint"
        );
        std::fs::write(entry.join("blobs/w"), b"weights").unwrap();
        assert_eq!(
            find_local_snapshot(&repo, models.path(), Some(cache.path())),
            Some(snap.clone())
        );

        // A pinned commit resolves without a ref; an unknown revision finds nothing.
        let pinned = HubRepo::parse(
            "https://huggingface.co/mlx-community/embeddinggemma-2-bf16/tree/abc123",
        )
        .unwrap();
        assert_eq!(
            find_local_snapshot(&pinned, models.path(), Some(cache.path())),
            Some(snap)
        );
        let other =
            HubRepo::parse("https://huggingface.co/mlx-community/embeddinggemma-2-bf16/tree/v2")
                .unwrap();
        assert_eq!(
            find_local_snapshot(&other, models.path(), Some(cache.path())),
            None
        );
    }

    #[test]
    fn auth_failures_and_cancel_are_reported() {
        let routes = HashMap::from([(
            "/api/models/o/gated/revision/main?blobs=true".to_string(),
            (401, b"{}".to_vec()),
        )]);
        let (base, _) = fake_hub(routes);
        let root = tempfile::tempdir().unwrap();
        let gated = HubRepo::parse(&format!("{base}/o/gated")).unwrap();
        let err = download(
            &gated,
            root.path(),
            None,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, HubError::Unauthorized { status: 401, .. }));
        assert!(err.to_string().contains("令牌"));
        let en = mm_i18n::with_lang(mm_i18n::Lang::En, || err.to_string());
        assert!(en.starts_with("No permission to download "), "{en}");
        assert!(
            en.contains("(HTTP 401)") && en.contains("Hugging Face token"),
            "{en}"
        );

        let missing = HubRepo::parse(&format!("{base}/o/missing")).unwrap();
        assert!(matches!(
            download(
                &missing,
                root.path(),
                None,
                &AtomicBool::new(false),
                &mut |_| {}
            ),
            Err(HubError::NotFound(_))
        ));

        let routes = HashMap::from([(
            "/api/models/o/m/revision/main?blobs=true".to_string(),
            (200, listing(&[("config.json", 2)])),
        )]);
        let (base, _) = fake_hub(routes);
        let repo = HubRepo::parse(&format!("{base}/o/m")).unwrap();
        assert!(matches!(
            download(
                &repo,
                root.path(),
                None,
                &AtomicBool::new(true),
                &mut |_| {}
            ),
            Err(HubError::Cancelled)
        ));
    }

    #[test]
    fn unsafe_listings_write_nothing() {
        let routes = HashMap::from([(
            "/api/models/o/m/revision/main?blobs=true".to_string(),
            (200, listing(&[("config.json", 2), ("../evil", 1)])),
        )]);
        let (base, _) = fake_hub(routes);
        let repo = HubRepo::parse(&format!("{base}/o/m")).unwrap();
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            download(
                &repo,
                root.path(),
                None,
                &AtomicBool::new(false),
                &mut |_| {}
            ),
            Err(HubError::UnsafePath(_))
        ));
        assert!(!root.path().join("o--m").exists());
    }

    /// Talks to the real Hub: lists the default repo and fetches its small `config.json` and
    /// `1_Pooling/config.json` through the `/resolve` redirect. Run with `--ignored`.
    #[test]
    #[ignore = "needs network access to huggingface.co"]
    fn real_hub_lists_and_fetches_small_files() {
        let repo = HubRepo::parse(DEFAULT_EMBEDDING_REPO_URL).unwrap();
        let agent = agent();
        let listing = list_files_with(&agent, &repo, None).unwrap();
        assert_eq!(listing.sha.len(), 40);
        let dest = tempfile::tempdir().unwrap();
        for name in ["config.json", "1_Pooling/config.json"] {
            let file = listing.files.iter().find(|f| f.path == name).unwrap();
            let target = local_path(dest.path(), name).unwrap();
            let mut cb = |_: &Progress| {};
            let mut rep = Reporter {
                progress: Progress::default(),
                last_sent: 0,
                on_progress: &mut cb,
            };
            fetch_file(
                &agent,
                &repo,
                &listing.sha,
                file,
                &target,
                None,
                &AtomicBool::new(false),
                &mut rep,
            )
            .unwrap();
            assert_eq!(Some(file_len(&target)), file.size);
        }
    }
}
