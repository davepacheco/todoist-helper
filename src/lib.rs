//! Shared library for resolving GitHub and RFD references to Markdown links.

use anyhow::{Context, anyhow};
use octocrab::{Octocrab, models::issues::Issue, models::pulls::PullRequest};
use regex::Regex;

/// URL for the RFD API
pub const RFD_API_URL: &str = "https://rfd-api.shared.oxide.computer";

/// Default GitHub organization for short-form repository references
/// like `omicron#123`
pub const DEFAULT_GITHUB_ORG: &str = "oxidecomputer";

/// Describes a parsed link to a GitHub issue or pull request
#[derive(Debug)]
pub struct GitHubLink {
    pub owner: String,
    pub repo: String,
    pub kind: GitHubKind,
    pub number: u64,
}

/// Kind of a GitHub link
#[derive(Debug)]
pub enum GitHubKind {
    Issue,
    PullRequest,
    /// Kind is unknown (from a short-form reference like `repo#123`)
    Unknown,
}

/// Resolved information about a GitHub issue or pull request
#[derive(Debug)]
pub struct GitHubWorkItem {
    /// link to the GitHub page for this item
    pub url: String,
    /// title of the item
    pub title: String,
    /// human-readable label (e.g., `owner/repo#123`)
    pub label: String,
}

/// Describes a parsed reference to an RFD
#[derive(Debug)]
pub struct RfdReference {
    pub number: u64,
}

/// Resolved information about an RFD
#[derive(Debug)]
pub struct RfdWorkItem {
    pub url: String,
    pub title: String,
    pub label: String,
}

/// Extract full GitHub issue/PR URLs from text
///
/// Matches patterns like `https://github.com/owner/repo/issues/123`.
pub fn extract_github_links(text: &str) -> Vec<GitHubLink> {
    let github_regex = Regex::new(concat!(
        r"https?://github\.com/",
        r"(?P<owner>[\w-]+)/(?P<repo>[\w-]+)/",
        r"(issues|pull)/(?P<number>\d+)",
    ))
    .unwrap();

    github_regex
        .captures_iter(text)
        .filter_map(|caps| {
            let owner = caps.name("owner")?.as_str().to_string();
            let repo = caps.name("repo")?.as_str().to_string();
            let number: u64 = caps.name("number")?.as_str().parse().ok()?;
            let kind = match caps.get(3)?.as_str() {
                "issues" => GitHubKind::Issue,
                "pull" => GitHubKind::PullRequest,
                _ => return None,
            };
            Some(GitHubLink { owner, repo, kind, number })
        })
        .collect()
}

/// Extract short-form GitHub references from text
///
/// Handles `repo#123` (implying [`DEFAULT_GITHUB_ORG`]) and
/// `owner/repo#123`.
pub fn extract_short_github_refs(text: &str) -> Vec<GitHubLink> {
    let short_regex =
        Regex::new(r"(?:(?P<owner>[\w-]+)/)?(?P<repo>[\w-]+)#(?P<number>\d+)")
            .unwrap();

    short_regex
        .captures_iter(text)
        .filter_map(|caps| {
            let owner = caps
                .name("owner")
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| DEFAULT_GITHUB_ORG.to_string());
            let repo = caps.name("repo")?.as_str().to_string();
            let number: u64 = caps.name("number")?.as_str().parse().ok()?;
            Some(GitHubLink { owner, repo, kind: GitHubKind::Unknown, number })
        })
        .collect()
}

/// Extract RFD references from text
///
/// Matches patterns like `RFD 123` or `RFD123`.
pub fn extract_rfd_references(text: &str) -> Vec<RfdReference> {
    let rfd_regex = Regex::new(r"RFD *(?P<number>\d+)").unwrap();
    rfd_regex
        .captures_iter(text)
        .filter_map(|caps| {
            let number: u64 = caps.name("number")?.as_str().parse().ok()?;
            Some(RfdReference { number })
        })
        .collect()
}

/// Fetch details for a GitHub issue or pull request
pub async fn fetch_github_work_item(
    octocrab: &Octocrab,
    link: &GitHubLink,
) -> anyhow::Result<GitHubWorkItem> {
    let label = format!("{}/{}#{}", link.owner, link.repo, link.number);
    match link.kind {
        GitHubKind::Issue | GitHubKind::Unknown => {
            let issue: Issue = octocrab
                .issues(link.owner.clone(), link.repo.clone())
                .get(link.number)
                .await
                .with_context(|| format!("failed to fetch {}", label))?;
            Ok(GitHubWorkItem {
                label,
                url: issue.html_url.to_string(),
                title: issue.title,
            })
        }
        GitHubKind::PullRequest => {
            let pr: PullRequest = octocrab
                .pulls(link.owner.clone(), link.repo.clone())
                .get(link.number)
                .await
                .with_context(|| format!("failed to fetch {}", label))?;
            let title = pr
                .title
                .ok_or_else(|| anyhow!("missing title for {}", label))?;
            let url = pr
                .html_url
                .ok_or_else(|| anyhow!("no HTML URL for {}", label))?
                .to_string();
            Ok(GitHubWorkItem { label, url, title })
        }
    }
}

/// Fetch details for an RFD reference
pub async fn fetch_rfd_work_item(
    rfd_client: &rfd_sdk::Client,
    rfd_ref: &RfdReference,
) -> anyhow::Result<RfdWorkItem> {
    let rfd_metadata = rfd_client
        .view_rfd_meta()
        .number(rfd_ref.number.to_string())
        .send()
        .await
        .with_context(|| {
            format!("failed to fetch information about RFD {}", rfd_ref.number,)
        })?
        .into_inner();

    let Ok(num) = u64::try_from(rfd_metadata.rfd_number) else {
        return Err(anyhow!(
            "RFD {}: reported RFD number was not a u64",
            rfd_metadata.rfd_number,
        ));
    };

    let title = rfd_metadata
        .title
        .ok_or_else(|| anyhow!("RFD {}: missing title", num))?;

    Ok(RfdWorkItem {
        url: format!("https://rfd.shared.oxide.computer/rfd/{}", num,),
        title,
        label: format!("RFD {}", num),
    })
}
