//! Ad-hoc resolver for GitHub and RFD references to Markdown links
//!
//! Accepts one or more references on the command line and prints a
//! Markdown-formatted link for each one found.  Supported formats:
//!
//! - Full GitHub URLs:    `https://github.com/owner/repo/issues/123`
//! - Short GitHub refs:   `repo#123` or `owner/repo#123`
//! - RFD references:      `RFD 123` or `RFD123`

static GITHUB_API_TOKEN: &str = include_str!("../../github_token.txt");
static RFD_API_TOKEN: &str = include_str!("../../rfd_site_token.txt");

#[tokio::main]
async fn main() {
    if let Err(error) = doit().await {
        eprintln!("error: {:#}", error);
        std::process::exit(1);
    }
}

async fn doit() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        anyhow::bail!(
            "usage: oxlink REFERENCE [...]\n\
             \n\
             REFERENCE may be a full GitHub URL, a short GitHub ref\n\
             (e.g., omicron#123 or oxidecomputer/omicron#123),\n\
             or an RFD reference (e.g., RFD 123)."
        );
    }

    // Set up client for talking to GitHub.
    let octocrab =
        todoist_helper::build_github_client(GITHUB_API_TOKEN.trim())?;

    // Set up client for talking to the RFD API.
    let rfd_client = todoist_helper::build_rfd_client(RFD_API_TOKEN.trim())?;

    for arg in &args {
        // Full GitHub URLs (e.g., https://github.com/owner/repo/issues/1)
        for link in todoist_helper::extract_github_links(arg) {
            match todoist_helper::fetch_github_work_item(&octocrab, &link).await
            {
                Ok(w) => println!("{}", w),
                Err(e) => eprintln!("warn: {:#}", e),
            }
        }

        // Short-form GitHub refs (e.g., omicron#123 or owner/repo#123).
        // Full URLs contain no `#number` patterns, so these two
        // extractors never overlap.
        for link in todoist_helper::extract_short_github_refs(arg) {
            match todoist_helper::fetch_github_work_item(&octocrab, &link).await
            {
                Ok(w) => println!("{}", w),
                Err(e) => eprintln!("warn: {:#}", e),
            }
        }

        // RFD references (e.g., RFD 123 or RFD123)
        for rfd_ref in todoist_helper::extract_rfd_references(arg) {
            match todoist_helper::fetch_rfd_work_item(&rfd_client, &rfd_ref)
                .await
            {
                Ok(w) => println!("{}", w),
                Err(e) => eprintln!("warn: {:#}", e),
            }
        }
    }

    Ok(())
}
