//! Command-line tool for constructing my status update from Todoist

use anyhow::{Context, anyhow};
use chrono::SecondsFormat;
use chrono::{DateTime, Utc};
use http::HeaderMap;
use http::HeaderValue;
use octocrab::Octocrab;
use reqwest::Client;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

// XXX-dap TODO:
// - get personal access token for Oxide organization
// - move the GitHub fetching to fetch-time instead of print-time so that
//   we can report all that stuff at once

static TODOIST_API_TOKEN: &str = include_str!("../todoist_token.txt");
static GITHUB_API_TOKEN: &str = include_str!("../github_token.txt");
static RFD_API_TOKEN: &str = include_str!("../rfd_site_token.txt");
// debug with: mitmproxy --mode reverse:https://api.todoist.com
// static TODOIST_API_URL: &str = "http://127.0.0.1:8080/api/v1";
static TODOIST_API_URL: &str = "https://api.todoist.com/api/v1";

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
    let mut rfd_headers = HeaderMap::new();
    rfd_headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", RFD_API_TOKEN.trim()))
            .context("constructing header")?,
    );

    let rfd_reqwest_client = reqwest::ClientBuilder::new()
        .default_headers(rfd_headers)
        .build()
        .context("failed to build reqwest client")?;

    let rfd_client = rfd_sdk::Client::new_with_client(
        todoist_helper::RFD_API_URL,
        rfd_reqwest_client,
    );

    // Fetch Todoist items
    let all_items = fetch_completed_tasks(&client, since).await?;

    // Print a report.  Along the way, fetch GitHub links and RFD links.
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

    // Store which tasks we've printed to avoid printing the same one
    // multiple times.  (This comes up for routines.)
    let mut printed = BTreeSet::new();

    println!("RECONFIGURATOR ITEMS:");
    for item in reconfigurator_items {
        if !printed.insert(&item.id) {
            continue;
        }
        println!("* {}", item.content);
        print_item_links(item, &octocrab, &rfd_client).await?;
    }

    println!("\n\nOther work:");

    for item in other_project_items {
        if !printed.insert(&item.id) {
            continue;
        }
        println!("* {}", item.content);
        print_item_links(item, &octocrab, &rfd_client).await?;
    }

    Ok(())
}

/// Print Markdown-formatted links for all GitHub and RFD references in
/// an item.
async fn print_item_links(
    item: &Item,
    octocrab: &Octocrab,
    rfd_client: &rfd_sdk::Client,
) -> anyhow::Result<()> {
    for link in todoist_helper::extract_github_links(&item.content) {
        match todoist_helper::fetch_github_work_item(octocrab, &link).await {
            Ok(w) => {
                println!("    * [{}]({}) ({:?})", w.label, w.url, w.title,)
            }
            Err(e) => eprintln!("warn: {:#}", e),
        }
    }
    for rfd_ref in todoist_helper::extract_rfd_references(&item.content) {
        match todoist_helper::fetch_rfd_work_item(rfd_client, &rfd_ref).await {
            Ok(w) => {
                println!("    * [{}]({}) ({:?})", w.label, w.url, w.title,)
            }
            Err(e) => eprintln!("warn: {:#}", e),
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

/// Fetch all items completed since `since`, grouped by each task's
/// project's name.
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
/// There can be many of these for one task if it's a recurring task that
/// was completed multiple times.
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
