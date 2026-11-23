use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::build_info;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateChannel {
    Stable,
    Nightly,
}

impl UpdateChannel {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(value: &str) -> Self {
        if value.eq_ignore_ascii_case("nightly") {
            Self::Nightly
        } else {
            Self::Stable
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Nightly => "nightly",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateState {
    Available,
    Current,
    LocalNewer,
    NoRelease,
    Unknown,
}

impl UpdateState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Current => "current",
            Self::LocalNewer => "local_newer",
            Self::NoRelease => "no_release",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug)]
pub struct UpdateStatus {
    pub channel: UpdateChannel,
    pub current_version: String,
    pub current_commit: String,
    pub latest_tag: String,
    pub latest_version: Option<String>,
    pub latest_commit: Option<String>,
    pub html_url: String,
    pub notes_preview: String,
    pub state: UpdateState,
    pub update_available: bool,
}

impl UpdateStatus {
    pub fn summary_line(&self) -> String {
        match self.state {
            UpdateState::Available => {
                format!("Update available: {} ({})", self.latest_tag, self.html_url)
            }
            UpdateState::Current => {
                format!("Up to date: {} ({})", self.current_version, self.latest_tag)
            }
            UpdateState::LocalNewer => format!("Local build is newer than {}", self.latest_tag),
            UpdateState::NoRelease => format!("No {} release is available", self.channel.as_str()),
            UpdateState::Unknown => {
                "Could not determine the relationship to the published nightly build".into()
            }
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::json!({
            "channel": self.channel.as_str(),
            "current_version": self.current_version,
            "current_commit": self.current_commit,
            "latest_tag": self.latest_tag,
            "latest_version": self.latest_version,
            "latest_commit": self.latest_commit,
            "html_url": self.html_url,
            "update_available": self.update_available,
            "status": self.state.as_str(),
        })
        .to_string()
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
}

struct Github {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
}

impl Github {
    fn get(&self, path: &str) -> Result<(Option<Value>, bool), String> {
        let mut request = self
            .agent
            .get(format!("{}/{path}", self.base))
            .header(
                "User-Agent",
                concat!("artinchip-flash/", env!("CARGO_PKG_VERSION")),
            )
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = request
            .call()
            .map_err(|error| format!("GitHub network or TLS error: {error}"))?;
        let status = response.status().as_u16();
        let headers = response.headers();
        if !response_available(status, headers)? {
            return Ok((None, false));
        }
        let has_next = headers
            .get("link")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|links| links.split(',').any(|link| link.contains("rel=\"next\"")));
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| format!("Could not read GitHub API response: {error}"))?;
        let data = serde_json::from_str(&body)
            .map_err(|error| format!("Invalid GitHub API response: {error}"))?;
        Ok((Some(data), has_next))
    }
}

fn response_available(status: u16, headers: &ureq::http::HeaderMap) -> Result<bool, String> {
    match status {
        200 => Ok(true),
        404 => Ok(false),
        401 => Err("GitHub authentication failed. Check GH_TOKEN or GITHUB_TOKEN.".into()),
        403 | 429
            if status == 429
                || headers
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0")
                || headers.contains_key("retry-after") =>
        {
            Err("GitHub API rate limit reached. Retry later.".into())
        }
        403 => Err("GitHub API denied access. Check token permissions or retry later.".into()),
        _ => Err(format!("GitHub API returned HTTP {status}")),
    }
}

pub fn check(channel: UpdateChannel) -> Result<UpdateStatus, String> {
    let repository = build_info::REPOSITORY
        .strip_prefix("https://github.com/")
        .ok_or("Invalid GitHub repository in application metadata")?;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .max_redirects(0)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let api = Github {
        agent,
        base: format!("https://api.github.com/repos/{repository}"),
        token: ["GH_TOKEN", "GITHUB_TOKEN"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.trim().is_empty()),
    };
    check_with(channel, |path| api.get(path))
}

fn check_with(
    channel: UpdateChannel,
    get: impl Fn(&str) -> Result<(Option<Value>, bool), String>,
) -> Result<UpdateStatus, String> {
    let mut result = UpdateStatus {
        channel,
        current_version: build_info::VERSION.into(),
        current_commit: build_info::COMMIT.into(),
        latest_tag: String::new(),
        latest_version: None,
        latest_commit: None,
        html_url: match channel {
            UpdateChannel::Stable => releases_url(),
            UpdateChannel::Nightly => nightly_url(),
        },
        notes_preview: String::new(),
        state: UpdateState::NoRelease,
        update_available: false,
    };
    match channel {
        UpdateChannel::Stable => {
            let mut releases = Vec::new();
            for page in 1.. {
                let (data, more) = get(&format!("releases?per_page=100&page={page}"))?;
                let data = data.ok_or(
                    "GitHub repository unavailable. Check the repository and token permissions.",
                )?;
                releases.extend(
                    serde_json::from_value::<Vec<Release>>(data)
                        .map_err(|error| error.to_string())?,
                );
                if !more {
                    break;
                }
            }
            if let Some((release, version)) = stable_release(&releases) {
                let current = semver::Version::parse(build_info::VERSION)
                    .map_err(|error| error.to_string())?;
                result.state = match version.cmp_precedence(&current) {
                    std::cmp::Ordering::Greater => UpdateState::Available,
                    std::cmp::Ordering::Equal => UpdateState::Current,
                    std::cmp::Ordering::Less => UpdateState::LocalNewer,
                };
                result.latest_tag = release.tag_name.clone();
                result.latest_version = Some(version.to_string());
                result.html_url = format!("{}/tag/{}", releases_url(), release.tag_name);
                result.notes_preview = preview(release.body.as_deref());
            }
        }
        UpdateChannel::Nightly => {
            if let (Some(data), _) = get("releases/tags/nightly")? {
                let release: Release =
                    serde_json::from_value(data).map_err(|error| error.to_string())?;
                if !release.draft && release.tag_name == "nightly" {
                    result.latest_tag = "nightly".into();
                    result.latest_commit =
                        nightly_commit(release.body.as_deref()).map(str::to_owned);
                    result.notes_preview = preview(release.body.as_deref());
                    result.state =
                        compare_nightly(build_info::COMMIT, result.latest_commit.as_deref(), get)?;
                }
            }
        }
    }
    result.update_available = result.state == UpdateState::Available;
    Ok(result)
}

fn stable_release(releases: &[Release]) -> Option<(&Release, semver::Version)> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let tag = &release.tag_name;
            let version = semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()?;
            version.pre.is_empty().then_some((release, version))
        })
        .max_by(|(_, left), (_, right)| left.cmp_precedence(right))
}

fn is_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn nightly_commit(body: Option<&str>) -> Option<&str> {
    body?
        .lines()
        .next()?
        .strip_prefix("Automated nightly build from `")?
        .strip_suffix("`.")
        .filter(|value| is_sha(value))
}

fn compare_nightly(
    local: &str,
    remote: Option<&str>,
    get: impl Fn(&str) -> Result<(Option<Value>, bool), String>,
) -> Result<UpdateState, String> {
    let Some(remote) = remote.filter(|_| is_sha(local)) else {
        return Ok(UpdateState::Unknown);
    };
    if local == remote {
        return Ok(UpdateState::Current);
    }
    let (comparison, _) = get(&format!("compare/{local}...{remote}"))?;
    Ok(
        match comparison
            .as_ref()
            .and_then(|value| value["status"].as_str())
        {
            Some("ahead") => UpdateState::Available,
            Some("behind") => UpdateState::LocalNewer,
            Some("identical") => UpdateState::Current,
            _ => UpdateState::Unknown,
        },
    )
}

fn preview(body: Option<&str>) -> String {
    body.unwrap_or_default()
        .lines()
        .take(8)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn is_portable_install() -> bool {
    std::env::current_exe().is_ok_and(|exe| {
        let path = exe.to_string_lossy().to_lowercase();
        ![
            "program files",
            "/usr/bin/",
            "/usr/local/bin/",
            "/applications/",
            "/opt/",
        ]
        .iter()
        .any(|managed| path.contains(managed))
    })
}

pub fn releases_url() -> String {
    format!("{}/releases", build_info::REPOSITORY)
}

pub fn nightly_url() -> String {
    format!("{}/tag/nightly", releases_url())
}

#[cfg(any(windows, unix))]
pub fn open_url(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let mut command;
    #[cfg(windows)]
    {
        command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler");
    }
    #[cfg(target_os = "macos")]
    {
        command = Command::new("open");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
        {
            return Err(format!(
                "No graphical browser session. Open {url} on another machine."
            ));
        }
        command = Command::new("xdg-open");
    }
    let status = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("Could not launch a browser: {error}. Open {url} manually."))?;
    if !status.success() {
        return Err(format!(
            "No browser accepted the request. Open {url} manually."
        ));
    }
    Ok(())
}

#[cfg(not(any(windows, unix)))]
pub fn open_url(url: &str) -> Result<(), String> {
    Err(format!("Open {url} manually in a browser."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn github_http_statuses_distinguish_absence_authentication_rate_limits_and_server_failure() {
        let mut headers = ureq::http::HeaderMap::new();
        assert!(!response_available(404, &headers).unwrap());
        assert!(response_available(401, &headers)
            .unwrap_err()
            .contains("authentication"));
        assert!(response_available(403, &headers)
            .unwrap_err()
            .contains("denied access"));
        assert!(response_available(503, &headers)
            .unwrap_err()
            .contains("503"));
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        for status in [403, 429] {
            assert!(response_available(status, &headers)
                .unwrap_err()
                .contains("rate limit"));
        }
    }

    #[test]
    fn stable_filters_release_flags_and_orders_semver() {
        let releases = json!([
            {"tag_name":"v10.0.0","draft":true,"prerelease":false},
            {"tag_name":"v9.0.0","draft":false,"prerelease":true},
            {"tag_name":"v8.0.0-rc.1","draft":false,"prerelease":false},
            {"tag_name":"nightly","draft":false,"prerelease":true},
            {"tag_name":"v0.2.10","draft":false,"prerelease":false},
            {"tag_name":"v0.2.9","draft":false,"prerelease":false},
        ]);
        let status = check_with(UpdateChannel::Stable, |path| {
            assert_eq!(path, "releases?per_page=100&page=1");
            Ok((Some(releases.clone()), false))
        })
        .unwrap();
        assert_eq!(status.state, UpdateState::Available);
        assert_eq!(status.latest_tag, "v0.2.10");
        for (response, expected) in [
            (json!([]), UpdateState::NoRelease),
            (
                json!([{"tag_name":"v0.2.1","draft":false,"prerelease":false}]),
                UpdateState::LocalNewer,
            ),
        ] {
            assert_eq!(
                check_with(UpdateChannel::Stable, |_| Ok((
                    Some(response.clone()),
                    false
                )))
                .unwrap()
                .state,
                expected
            );
        }
    }

    #[test]
    fn nightly_compares_published_sha_in_commit_order() {
        let local = "a".repeat(40);
        let remote = "b".repeat(40);
        let metadata = format!("Automated nightly build from `{remote}`.\n");
        assert_eq!(nightly_commit(Some(&metadata)), Some(remote.as_str()));
        assert_eq!(nightly_commit(Some("nightly from main")), None);
        assert_eq!(
            compare_nightly(&local, Some(&local), |_| unreachable!()).unwrap(),
            UpdateState::Current
        );
        assert_eq!(
            compare_nightly("unknown", Some(&remote), |_| unreachable!()).unwrap(),
            UpdateState::Unknown
        );
        for (relation, state) in [
            ("ahead", UpdateState::Available),
            ("behind", UpdateState::LocalNewer),
            ("diverged", UpdateState::Unknown),
        ] {
            assert_eq!(
                compare_nightly(&local, Some(&remote), |path| {
                    assert_eq!(path, format!("compare/{local}...{remote}"));
                    Ok((Some(json!({"status":relation})), false))
                })
                .unwrap(),
                state
            );
        }
    }
}
