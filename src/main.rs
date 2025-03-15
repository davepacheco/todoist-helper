//! command-line tool for constructing my status update from Todoist

use anyhow::{Context, anyhow};
use chrono::SecondsFormat;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use http::HeaderMap;
use http::HeaderValue;
use octocrab::{Octocrab, models::issues::Issue, models::pulls::PullRequest};
use regex::Regex;
use reqwest::Client;
use serde::Deserialize;
use std::collections::BTreeMap;

// XXX-dap TODO:
// - command-line argument for "since" date
// - do something similar for RFD URLs that I do for GitHub ones
// - get personal access token for Oxide organization
// - ask about some kind of access token for RFD site?

static TODOIST_API_TOKEN: &str = include_str!("../todoist_token.txt");
static GITHUB_API_TOKEN: &str = include_str!("../github_token.txt");
static API_URL: &str = "https://api.todoist.com/sync/v9";
// static API_URL: &str = "http://127.0.0.1:8080/sync/v9";

#[tokio::main]
async fn main() {
    if let Err(error) = doit().await {
        eprintln!("error: {:#}", error);
        std::process::exit(1);
    }
}

async fn doit() -> Result<(), anyhow::Error> {
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

    let octocrab = Octocrab::builder()
        .personal_token(GITHUB_API_TOKEN.trim())
        .build()
        .context("Failed to create Octocrab instance")?;

    // XXX-dap command-line arg, and figure out if it should be a day or what,
    // and if it's inclusive or what, and interpreted in what TZ
    // let since: chrono::DateTime<chrono::Utc> =
    //     "2025-03-13T00:00:00Z".parse().unwrap();
    let since: chrono::DateTime<chrono::Utc> =
        "2025-02-07T00:00:00-08:00".parse().unwrap();

    let mut all_items = fetch_completed_tasks(&client, since).await?;

    // Print a report.
    let (reconfigurator_project, reconfigurator_items) = all_items
        .iter()
        .find(|(k, _)| k.starts_with("Oxide: Reconfigurator"))
        .ok_or_else(|| anyhow!("failed to identify Reconfigurator project"))?;
    let other_project_items = all_items
        .iter()
        .filter_map(|(k, v)| {
            if k.starts_with("Oxide:") && k != reconfigurator_project {
                Some(v)
            } else {
                None
            }
        })
        .flatten();

    let mut printed = BTreeSet::new();

    for item in reconfigurator_items {
        if !printed.insert(item.id) {
            continue;
        }
        println!("* {}", item.content);
        for link in item.fetch_github_titles(&octocrab).await? {
            println!("    * [{}]({}) {}", link.label, link.link, link.title);
        }
    }

    println!();

    for item in other_project_items {
        if !printed.insert(item.id) {
            continue;
        }
        println!("* {}", item.content);
        for link in item.fetch_github_titles(&octocrab).await? {
            println!("    * [{}]({}) {}", link.label, link.link, link.title);
        }
    }

    Ok(())
}

async fn fetch_completed_tasks(
    client: &Client,
    since: DateTime<Utc>,
) -> anyhow::Result<BTreeMap<String, Vec<Item>>> {
    let mut offset = 0;
    let limit = 200;

    let mut rv = BTreeMap::new();

    loop {
        eprintln!("note: making Todoist request (offset = {offset})");
        let url = format!(
            "{}/completed/get_all?limit={}&offset={}&since={}",
            API_URL,
            limit,
            offset,
            since.to_rfc3339_opts(SecondsFormat::Secs, true),
        );

        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to send request to Todoist API")?;

        let completed_response: CompletedItems = response
            .json()
            .await
            .context("Failed to parse JSON response from Todoist API:\n{}\n")?;

        let nitems = completed_response.items.len();
        for item in completed_response.items {
            let Some(project) =
                completed_response.projects.get(&item.project_id)
            else {
                eprintln!(
                    "warning: item missing associated project in response"
                );
                continue;
            };

            let items = rv.entry(project.name.clone()).or_insert_with(Vec::new);
            items.push(item);
        }

        if nitems < limit {
            break;
        }

        offset += limit;
    }

    Ok(rv)
}

#[derive(Debug, Deserialize)]
struct Item {
    content: String,
    task_id: String,
    project_id: String,
    completed_at: DateTime<Utc>,
    id: String,
}

#[derive(Debug, Deserialize)]
struct CompletedItems {
    projects: BTreeMap<String, Project>,
    items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct Project {
    parent_id: Option<String>,
    id: String,
    name: String,
}

/// Struct to represent a parsed GitHub issue or pull request link
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

#[derive(Debug)]
struct GitHubWorkItem {
    link: String,
    title: String,
    label: String,
}

impl Item {
    /// Extract GitHub issue and pull request links as structured data
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

    /// Fetch the titles of GitHub issues or PRs using Octocrab
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
                        link: issue.html_url.to_string(),
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
                    rv.push(GitHubWorkItem {
                        label,
                        link: pr.html_url.to_string(),
                        title,
                    });
                }
            }
        }

        Ok(rv)
    }
}
