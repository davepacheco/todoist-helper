//! command-line tool for constructing my status update from Todoist

use anyhow::{Context, anyhow};
use chrono::SecondsFormat;
use chrono::{DateTime, Utc};
use http::HeaderMap;
use http::HeaderValue;
use octocrab::{Octocrab, models::issues::Issue, models::pulls::PullRequest};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

// XXX-dap TODO:
// - get personal access token for Oxide organization
// - move the GitHub fetching to fetch-time instead of print-time so that we can
//   report all that stuff at once

static TODOIST_API_TOKEN: &str = include_str!("../todoist_token.txt");
static GITHUB_API_TOKEN: &str = include_str!("../github_token.txt");
static RFD_API_TOKEN: &str = include_str!("../rfd_site_token.txt");
// debug with: mitmproxy --mode reverse:https://api.todoist.com
// static TODOIST_API_URL: &str = "http://127.0.0.1:8080/api/v1";
static TODOIST_API_URL: &str = "https://api.todoist.com/api/v1";
static RFD_API_URL: &str = "https://rfd-api.shared.oxide.computer";

#[tokio::main]
async fn main() {
    if let Err(error) = doit().await {
        eprintln!("error: {:#}", error);
        std::process::exit(1);
    }
}

async fn doit() -> Result<(), anyhow::Error> {
    // Parse the "since" argument.
    let since_arg = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("expected TIMESTAMP argument"))?;
    let since: DateTime<Utc> = DateTime::parse_from_rfc3339(&since_arg)
        .context("expected RFC 3339 timestamp")?
        .to_utc();

    // Set up client for talking to Todoist
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", TODOIST_API_TOKEN.trim()))
            .context("constructing header")?,
    );

    let client = reqwest::ClientBuilder::new()
        .default_headers(headers)
        .build()
        .context("failed to build reqwest client")?;

    // Set up client for talking to GitHub
    let octocrab = Octocrab::builder()
        .personal_token(GITHUB_API_TOKEN.trim())
        .build()
        .context("Failed to create Octocrab instance")?;

    // Set up client for talking to RFD API
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", RFD_API_TOKEN.trim()))
            .context("constructing header")?,
    );

    let rfd_reqwest_client = reqwest::ClientBuilder::new()
        .default_headers(headers)
        .build()
        .context("failed to build reqwest client")?;

    let rfd_client =
        rfd_sdk::Client::new_with_client(RFD_API_URL, rfd_reqwest_client);

    // Fetch Todoist items
    let all_items = fetch_completed_tasks(&client, since).await?;

    // Print a report.  Along the way, fetch GItHub links and RFD links.
    let (reconfigurator_project, reconfigurator_items) = all_items
        .iter()
        .find(|(k, _)| k.starts_with("Oxide: Reconfigurator"))
        .ok_or_else(|| anyhow!("failed to identify Reconfigurator project"))?;
    let other_project_items = all_items
        .iter()
        .filter_map(|(k, v)| {
            if k.starts_with("Oxide") && k != reconfigurator_project {
                Some(v)
            } else {
                None
            }
        })
        .flatten();

    // Store which tasks we've printed to avoid printing the same one multiple
    // times.  (This comes up for routines.)
    let mut printed = BTreeSet::new();

    println!("RECONFIGURATOR ITEMS:");
    for item in reconfigurator_items {
        if !printed.insert(&item.id) {
            continue;
        }
        println!("* {}", item.content);
        for link in item.fetch_github_titles(&octocrab).await? {
            println!("    * [{}]({}) ({:?})", link.label, link.url, link.title);
        }

        for link in item.fetch_rfd_titles(&rfd_client).await? {
            println!("    * [{}]({}) ({:?})", link.label, link.url, link.title);
        }
    }

    println!("\n\nOther work:");

    for item in other_project_items {
        if !printed.insert(&item.id) {
            continue;
        }
        println!("* {}", item.content);
        for link in item.fetch_github_titles(&octocrab).await? {
            println!("    * [{}]({}) ({:?})", link.label, link.url, link.title);
        }
        for link in item.fetch_rfd_titles(&rfd_client).await? {
            println!("    * [{}]({}) ({:?})", link.label, link.url, link.title);
        }
    }

    Ok(())
}

/// Fetch all Todoist projects, returning a map from project ID to project.
async fn fetch_projects(
    client: &Client,
) -> anyhow::Result<BTreeMap<String, Project>> {
    let mut rv = BTreeMap::new();
    let mut cursor: Option<String> = None;

    loop {
        eprintln!("note: making Todoist projects request");

        let mut request = client
            .get(format!("{}/projects", TODOIST_API_URL))
            .query(&[("limit", "10")]);
        if let Some(ref c) = cursor {
            request = request.query(&[("cursor", c.as_str())]);
        }

        let response = request
            .send()
            .await
            .context("Failed to send request to Todoist projects API")?;

        let page: ProjectsPage = response.json().await.context(
            "Failed to parse JSON response from Todoist projects API",
        )?;

        let next = page.next_cursor;
        for project in page.results {
            rv.insert(project.id.clone(), project);
        }

        match next {
            None => break,
            Some(c) => cursor = Some(c),
        }
    }

    Ok(rv)
}

/// From Todoist, fetch all items completed since `since`, grouped by each
/// task's project's name.
async fn fetch_completed_tasks(
    client: &Client,
    since: DateTime<Utc>,
) -> anyhow::Result<BTreeMap<String, Vec<Item>>> {
    let projects = fetch_projects(client).await?;
    let since_str = since.to_rfc3339_opts(SecondsFormat::Secs, true);
    let until_str = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut rv = BTreeMap::new();
    let mut cursor: Option<String> = None;

    loop {
        eprintln!("note: making Todoist request");

        let mut request = client
            .get(format!(
                "{}/tasks/completed/by_completion_date",
                TODOIST_API_URL,
            ))
            .query(&[
                ("limit", "200"),
                ("since", &since_str),
                ("until", &until_str),
            ]);

        if let Some(ref c) = cursor {
            request = request.query(&[("cursor", c.as_str())]);
        }

        let response = request
            .send()
            .await
            .context("Failed to send request to Todoist API")?;

        let page: CompletedTasksPage = response
            .json()
            .await
            .context("Failed to parse JSON response from Todoist API")?;

        let next = page.next_cursor;
        for item in page.items {
            let Some(project) = projects.get(&item.project_id) else {
                eprintln!(
                    "warning: item {:?} missing associated project",
                    item.id,
                );
                continue;
            };

            rv.entry(project.name.clone()).or_insert_with(Vec::new).push(item);
        }

        match next {
            None => break,
            Some(c) => cursor = Some(c),
        }
    }

    Ok(rv)
}

/// Describes the response to the "get completed tasks" API
#[derive(Debug, Deserialize)]
struct CompletedTasksPage {
    /// list of completed tasks
    items: Vec<Item>,
    /// cursor for the next page, or `None` if there are no more pages
    next_cursor: Option<String>,
}

/// Describes one completed item
///
/// There can be many of these for one task if it's a recurring task that was
/// completed multiple times.
#[derive(Debug, Deserialize)]
struct Item {
    content: String,
    id: String,
    project_id: String,
}

/// Describes the response to the "get projects" API
#[derive(Debug, Deserialize)]
struct ProjectsPage {
    /// list of projects
    results: Vec<Project>,
    /// cursor for the next page, or `None` if there are no more pages
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Project {
    id: String,
    name: String,
}

/// Describes a parsed link to a GitHub issue or pull request
#[derive(Debug)]
struct GitHubLink {
    owner: String,
    repo: String,
    kind: GitHubKind,
    number: u64,
}

#[derive(Debug)]
enum GitHubKind {
    Issue,
    PullRequest,
}

/// Summarizes the information about a completed GitHub item
#[derive(Debug)]
struct GitHubWorkItem {
    /// link to the GitHub page for this item
    url: String,
    /// title of the item
    title: String,
    /// human-readable summary of the item (generally: `owner/repo#123`)
    label: String,
}

/// Describes a parsed reference to an RFD
#[derive(Debug)]
struct RfdReference {
    number: u64,
}

/// Summarizes information about a referenced RFD item
#[derive(Debug)]
struct RfdWorkItem {
    url: String,
    title: String,
    label: String,
}

impl Item {
    /// Extract GitHub issue and pull request links
    fn extract_github_links(&self) -> Vec<GitHubLink> {
        let github_regex = Regex::new(
            r"https?://github\.com/(?P<owner>[\w-]+)/(?P<repo>[\w-]+)/(issues|pull)/(?P<number>\d+)"
        )
        .unwrap();

        github_regex
            .captures_iter(&self.content)
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

    /// Fetch the titles of GitHub issues or PRs mentioned in this item
    async fn fetch_github_titles(
        &self,
        octocrab: &Octocrab,
    ) -> anyhow::Result<Vec<GitHubWorkItem>> {
        let mut rv = Vec::new();
        for link in self.extract_github_links() {
            let label = format!("{}/{}#{}", link.owner, link.repo, link.number);
            // eprintln!("note: fetching title for {}", label);
            match link.kind {
                GitHubKind::Issue => {
                    let issue: Issue = match octocrab
                        .issues(link.owner.clone(), link.repo.clone())
                        .get(link.number)
                        .await
                        .context(format!("Failed to fetch {}", label))
                    {
                        Ok(i) => i,
                        Err(e) => {
                            eprintln!("warn: {:#}", e);
                            continue;
                        }
                    };
                    rv.push(GitHubWorkItem {
                        label,
                        url: issue.html_url.to_string(),
                        title: issue.title,
                    });
                }
                GitHubKind::PullRequest => {
                    let pr: PullRequest = match octocrab
                        .pulls(link.owner.clone(), link.repo.clone())
                        .get(link.number)
                        .await
                        .context(format!("Failed to fetch {}", label))
                    {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("warn: {:#}", e);
                            continue;
                        }
                    };
                    let title = pr.title.ok_or_else(|| {
                        anyhow!("Missing title for {}", label)
                    })?;
                    let url = match pr.html_url {
                        Some(u) => u.to_string(),
                        None => {
                            eprintln!("warn: no HTML url for {}", label);
                            continue;
                        }
                    };
                    rv.push(GitHubWorkItem { label, url, title });
                }
            }
        }

        Ok(rv)
    }

    /// Extract references to RFD numbers
    fn extract_rfd_references(&self) -> Vec<RfdReference> {
        let rfd_regex = Regex::new(r"RFD *(?P<number>\d+)").unwrap();

        rfd_regex
            .captures_iter(&self.content)
            .filter_map(|caps| {
                let number: u64 = caps.name("number")?.as_str().parse().ok()?;
                Some(RfdReference { number })
            })
            .collect()
    }

    /// Fetch the titles of GitHub issues or PRs mentioned in this item
    async fn fetch_rfd_titles(
        &self,
        rfd_client: &rfd_sdk::Client,
    ) -> anyhow::Result<Vec<RfdWorkItem>> {
        let mut rv = Vec::new();
        for rfdref in self.extract_rfd_references() {
            let rfd_metadata = rfd_client
                .view_rfd_meta()
                .number(rfdref.number.to_string())
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed to fetch information about RFD {}",
                        rfdref.number
                    )
                });
            let rfd_metadata = match rfd_metadata {
                Err(error) => {
                    eprintln!("warn: {:#}", error);
                    continue;
                }
                Ok(metadata) => metadata.into_inner(),
            };

            let Ok(num) = u64::try_from(rfd_metadata.rfd_number) else {
                eprintln!(
                    "warn: RFD {}: reported RFD number was not a u64",
                    rfd_metadata.rfd_number
                );
                continue;
            };

            let Some(title) = rfd_metadata.title else {
                eprintln!("warn: RFD {}: missing title", num);
                continue;
            };

            rv.push(RfdWorkItem {
                url: format!("https://rfd.shared.oxide.computer/rfd/{}", num),
                title: title.clone(),
                label: format!("RFD {}", num),
            });
        }

        Ok(rv)
    }
}
