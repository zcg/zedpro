use crate::{AsyncBody, HttpClient, HttpRequestExt};
use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt;
use http::{Method, Request, StatusCode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;
use url::Url;

const GITHUB_API_URL: &str = "https://api.github.com";

pub struct GitHubLspBinaryVersion {
    pub name: String,
    pub url: String,
    pub digest: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(rename = "prerelease")]
    pub pre_release: bool,
    pub assets: Vec<GithubReleaseAsset>,
    pub tarball_url: String,
    pub zipball_url: String,
}

#[derive(Deserialize, Debug)]
pub struct GithubReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub digest: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct GithubUser {
    pub login: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct GithubRepoOwner {
    pub login: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct GithubRepo {
    pub name: String,
    pub private: bool,
    pub html_url: String,
    pub default_branch: String,
    pub owner: GithubRepoOwner,
}

#[derive(Deserialize, Debug, Clone)]
pub struct GithubContentFile {
    pub path: String,
    pub sha: String,
    pub encoding: Option<String>,
    pub content: Option<String>,
    pub download_url: Option<String>,
}

pub async fn latest_github_release(
    repo_name_with_owner: &str,
    require_assets: bool,
    pre_release: bool,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<GithubRelease> {
    let url = format!("{GITHUB_API_URL}/repos/{repo_name_with_owner}/releases");

    let request = Request::get(&url)
        .follow_redirects(crate::RedirectPolicy::FollowAll)
        .when_some(std::env::var("GITHUB_TOKEN").ok(), |builder, token| {
            builder.header("Authorization", format!("Bearer {}", token))
        })
        .body(Default::default())?;

    let mut response = http
        .send(request)
        .await
        .context("error fetching latest release")?;

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("error reading latest release")?;

    if response.status().is_client_error() {
        let text = String::from_utf8_lossy(body.as_slice());
        bail!(
            "status error {}, response: {text:?}",
            response.status().as_u16()
        );
    }

    let releases = match serde_json::from_slice::<Vec<GithubRelease>>(body.as_slice()) {
        Ok(releases) => releases,

        Err(err) => {
            log::error!("Error deserializing: {err:?}");
            log::error!(
                "GitHub API response text: {:?}",
                String::from_utf8_lossy(body.as_slice())
            );
            anyhow::bail!("error deserializing latest release: {err:?}");
        }
    };

    let mut release = releases
        .into_iter()
        .filter(|release| !require_assets || !release.assets.is_empty())
        .find(|release| release.pre_release == pre_release)
        .context("finding a prerelease")?;
    release.assets.iter_mut().for_each(|asset| {
        if let Some(digest) = &mut asset.digest
            && let Some(stripped) = digest.strip_prefix("sha256:")
        {
            *digest = stripped.to_owned();
        }
    });
    Ok(release)
}

pub async fn get_release_by_tag_name(
    repo_name_with_owner: &str,
    tag: &str,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<GithubRelease> {
    let url = format!("{GITHUB_API_URL}/repos/{repo_name_with_owner}/releases/tags/{tag}");

    let request = Request::get(&url)
        .follow_redirects(crate::RedirectPolicy::FollowAll)
        .when_some(std::env::var("GITHUB_TOKEN").ok(), |builder, token| {
            builder.header("Authorization", format!("Bearer {}", token))
        })
        .body(Default::default())?;

    let mut response = http
        .send(request)
        .await
        .context("error fetching latest release")?;

    let mut body = Vec::new();
    let status = response.status();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("error reading latest release")?;

    if status.is_client_error() {
        let text = String::from_utf8_lossy(body.as_slice());
        bail!(
            "status error {}, response: {text:?}",
            response.status().as_u16()
        );
    }

    let release = serde_json::from_slice::<GithubRelease>(body.as_slice()).map_err(|err| {
        log::error!("Error deserializing: {err:?}");
        log::error!(
            "GitHub API response text: {:?}",
            String::from_utf8_lossy(body.as_slice())
        );
        anyhow!("error deserializing GitHub release: {err:?}")
    })?;

    Ok(release)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AssetKind {
    TarGz,
    Gz,
    Zip,
}

pub fn build_asset_url(repo_name_with_owner: &str, tag: &str, kind: AssetKind) -> Result<String> {
    let mut url = Url::parse(&format!(
        "https://github.com/{repo_name_with_owner}/archive/refs/tags",
    ))?;
    // We're pushing this here, because tags may contain `/` and other characters
    // that need to be escaped.
    let asset_filename = format!(
        "{tag}.{extension}",
        extension = match kind {
            AssetKind::TarGz => "tar.gz",
            AssetKind::Gz => "gz",
            AssetKind::Zip => "zip",
        }
    );
    url.path_segments_mut()
        .map_err(|()| anyhow!("cannot modify url path segments"))?
        .push(&asset_filename);
    Ok(url.to_string())
}

pub async fn current_user(token: &str, http: Arc<dyn HttpClient>) -> anyhow::Result<GithubUser> {
    send_github_request(Method::GET, &format!("{GITHUB_API_URL}/user"), token, None::<()>, http)
        .await
}

pub async fn get_repo(
    owner: &str,
    repo: &str,
    token: &str,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<Option<GithubRepo>> {
    send_optional_github_request(
        Method::GET,
        &format!("{GITHUB_API_URL}/repos/{owner}/{repo}"),
        token,
        None::<()>,
        http,
    )
    .await
}

pub async fn create_private_repo(
    repo: &str,
    token: &str,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<GithubRepo> {
    #[derive(Serialize)]
    struct CreateRepoBody<'a> {
        name: &'a str,
        private: bool,
        auto_init: bool,
        description: &'a str,
    }

    send_github_request(
        Method::POST,
        &format!("{GITHUB_API_URL}/user/repos"),
        token,
        Some(CreateRepoBody {
            name: repo,
            private: true,
            auto_init: true,
            description: "ZedPro settings sync repository",
        }),
        http,
    )
    .await
}

pub async fn get_repo_content(
    owner: &str,
    repo: &str,
    path: &str,
    token: &str,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<Option<GithubContentFile>> {
    send_optional_github_request(
        Method::GET,
        &github_contents_url(owner, repo, path)?,
        token,
        None::<()>,
        http,
    )
    .await
}

pub async fn put_repo_content(
    owner: &str,
    repo: &str,
    path: &str,
    message: &str,
    content_base64: String,
    sha: Option<String>,
    token: &str,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<()> {
    #[derive(Serialize)]
    struct PutContentBody<'a> {
        message: &'a str,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>,
    }

    let _: serde_json::Value = send_github_request(
        Method::PUT,
        &github_contents_url(owner, repo, path)?,
        token,
        Some(PutContentBody {
            message,
            content: content_base64,
            sha,
        }),
        http,
    )
    .await?;
    Ok(())
}

async fn send_optional_github_request<T, B>(
    method: Method,
    url: &str,
    token: &str,
    body: Option<B>,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<Option<T>>
where
    T: DeserializeOwned,
    B: Serialize,
{
    match send_github_request(method, url, token, body, http).await {
        Ok(value) => Ok(Some(value)),
        Err(err) if is_not_found_error(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

async fn send_github_request<T, B>(
    method: Method,
    url: &str,
    token: &str,
    body: Option<B>,
    http: Arc<dyn HttpClient>,
) -> anyhow::Result<T>
where
    T: DeserializeOwned,
    B: Serialize,
{
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .follow_redirects(crate::RedirectPolicy::FollowAll)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28");

    if let Some(user_agent) = http.user_agent() {
        builder = builder.header("User-Agent", user_agent.clone());
    }

    let request = builder.body(match body {
        Some(body) => AsyncBody::from(serde_json::to_vec(&body)?),
        None => AsyncBody::default(),
    })?;

    let mut response = http
        .send(request)
        .await
        .with_context(|| format!("error sending GitHub API request to {url}"))?;
    let status = response.status();

    let mut raw_body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut raw_body)
        .await
        .with_context(|| format!("error reading GitHub API response from {url}"))?;

    if status.is_client_error() || status.is_server_error() {
        let text = String::from_utf8_lossy(raw_body.as_slice());
        let prefix = if status == StatusCode::NOT_FOUND {
            "github_not_found"
        } else {
            "github_request_failed"
        };
        bail!("{prefix}: status {}, response: {text}", status.as_u16());
    }

    serde_json::from_slice::<T>(raw_body.as_slice()).map_err(|err| {
        log::error!("Error deserializing GitHub API response: {err:?}");
        log::error!(
            "GitHub API response text: {:?}",
            String::from_utf8_lossy(raw_body.as_slice())
        );
        anyhow!("error deserializing GitHub API response: {err:?}")
    })
}

fn is_not_found_error(err: &anyhow::Error) -> bool {
    err.to_string().contains("github_not_found")
}

fn github_contents_url(owner: &str, repo: &str, path: &str) -> Result<String> {
    let mut url = Url::parse(&format!("{GITHUB_API_URL}/repos/{owner}/{repo}/contents"))?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| anyhow!("cannot modify GitHub contents URL path"))?;
    for segment in path.split('/') {
        segments.push(segment);
    }
    drop(segments);
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use crate::github::{AssetKind, build_asset_url};

    #[test]
    fn test_build_asset_url() {
        let tag = "release/2.3.5";
        let repo_name_with_owner = "microsoft/vscode-eslint";

        let tarball = build_asset_url(repo_name_with_owner, tag, AssetKind::TarGz).unwrap();
        assert_eq!(
            tarball,
            "https://github.com/microsoft/vscode-eslint/archive/refs/tags/release%2F2.3.5.tar.gz"
        );

        let zip = build_asset_url(repo_name_with_owner, tag, AssetKind::Zip).unwrap();
        assert_eq!(
            zip,
            "https://github.com/microsoft/vscode-eslint/archive/refs/tags/release%2F2.3.5.zip"
        );
    }
}
