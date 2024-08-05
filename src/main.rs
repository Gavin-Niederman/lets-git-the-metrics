use std::{collections::BTreeMap, error::Error};

use clap::Parser;
use reqwest::{Client, IntoUrl, Response};
use serde::Deserialize;

#[derive(clap::Parser, Clone)]
struct Args {
    #[arg(long, short)]
    user: String,
    #[arg(long, short)]
    token: Option<String>,
    #[arg(long, short)]
    weighted: bool,

    #[arg(long, short, default_value = "")]
    excluded_langs: Vec<String>,
}

struct GitHub {
    client: Client,
    user: String,
    auth_code: Option<String>,
    weighted: bool,
    excluded_langs: Vec<String>,
}
impl GitHub {
    pub fn from_args(args: Args) -> Self {
        Self {
            client: Client::new(),
            user: args.user,
            auth_code: args.token,
            weighted: args.weighted,
            excluded_langs: args
                .excluded_langs
                .into_iter()
                .map(|s| s.to_ascii_lowercase())
                .collect(),
        }
    }

    pub async fn user_data(&self) -> Result<UserData, Box<dyn Error>> {
        let json = self
            .get(format!("https://api.github.com/users/{}", self.user))
            .await?
            .text()
            .await?;
        let data: UserData = serde_json::from_str(&json).unwrap();
        Ok(data)
    }

    pub async fn get(&self, url: impl IntoUrl) -> reqwest::Result<Response> {
        let mut builder = self
            .client
            .get(format!("{}?per_page=1000", url.as_str()))
            .header("User-Agent", "GitHub user stats scraper (reqwest/hyper)");
        if let Some(auth) = &self.auth_code {
            builder = builder.header("Authorization", format!("Bearer {auth}"));
        }
        builder.send().await
    }
}

async fn collect_repos(connection: &GitHub) -> Result<Vec<RepoData>, Box<dyn Error>> {
    let user_data = connection.user_data().await?;

    println!(
        "Successfully found user. scraping repos at `{}` and organizations at `{}`...",
        user_data.repos_url, user_data.organizations_url
    );

    let repos_data = connection.get(user_data.repos_url).await?.text().await?;
    let mut repos: Vec<RepoData> = serde_json::from_str(&repos_data).unwrap();
    println!("Found all {} user repos!", repos.len());

    let orgs_data = connection
        .get(user_data.organizations_url)
        .await?
        .text()
        .await?;
    let orgs_data: Vec<OrgData> = serde_json::from_str(&orgs_data).unwrap();
    for org in orgs_data {
        let repos_data = connection.get(org.repos_url).await?.text().await?;
        let repos_data: Vec<RepoData> = serde_json::from_str(&repos_data).unwrap();
        println!("Found {} organization repos!", repos_data.len());
        repos.extend(repos_data)
    }

    Ok(repos)
}

struct RepoInfo {
    language_loc_map: BTreeMap<String, u32>,
    ratio_of_commits_from_user: f32,
    stars: u32,
}

async fn handle_repo(
    repo: RepoData,
    connection: &GitHub,
) -> Result<Option<RepoInfo>, Box<dyn Error>> {
    // Get the ratio of all contributions to contributions from the user
    let contributors_json = connection.get(&repo.contributors_url).await?.text().await?;
    let Ok(contributors) = serde_json::from_str::<Vec<ContributorData>>(&contributors_json) else {
        return Ok(None);
    };

    let total_contributions = contributors
        .iter()
        .map(|data| data.contributions)
        .sum::<u32>();
    let Some(user_contributor) = contributors.iter().find(|contributor| {
        contributor.login.to_ascii_lowercase() == connection.user.to_ascii_lowercase()
    }) else {
        return Ok(None);
    };

    let ratio_of_contributions = user_contributor.contributions as f32 / total_contributions as f32;

    // Get the ratio of all languages in the repo
    let langs_json = connection
        .client
        .get(format!(
            "https://api.codetabs.com/v1/loc/?github={}",
            &repo.full_name
        ))
        .send()
        .await?
        .text()
        .await?;
    let Ok(langs) = serde_json::from_str::<Vec<LOCData>>(&langs_json) else {
        println!("Failed to get language data for repo: {}", repo.full_name);
        return Ok(None);
    };

    let language_loc_map: BTreeMap<String, u32> = langs
        .into_iter()
        .filter(|data| {
            data.language != "Total"
                && !connection
                    .excluded_langs
                    .contains(&data.language.to_ascii_lowercase())
        })
        .map(|data| (data.language, data.lines_of_code))
        .collect();

    let stars = repo.stargazers_count;

    println!("Processed new repo: {}! {stars} stars found with {:.2}% of contributions being from selected user.", repo.full_name, ratio_of_contributions * 100.0);
    Ok(Some(RepoInfo {
        language_loc_map,
        ratio_of_commits_from_user: ratio_of_contributions,
        stars,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let connection = GitHub::from_args(args);

    println!("Excluding languages: {:?}", connection.excluded_langs);

    let repos = collect_repos(&connection).await?;

    // Get meaningful data from repos and filter
    let mut repos_info = Vec::new();
    for repo in repos {
        let Some(info) = handle_repo(repo, &connection).await? else {
            continue;
        };
        repos_info.push(info);
    }

    // Sum all language ratios into a new map
    let mut langs_map: BTreeMap<String, f32> = BTreeMap::new();
    for info in repos_info.iter() {
        for (lang, val) in info.language_loc_map.clone() {
            let val = if connection.weighted {
                val as f32 * info.ratio_of_commits_from_user
            } else {
                val as f32
            };
            if let Some(old) = langs_map.get(&lang) {
                let new = old + val;
                langs_map.insert(lang, new);
            } else {
                langs_map.insert(lang, val);
            }
        }
    }

    // Scale so that all values add to 100
    let sum_of_components = langs_map.values().sum::<f32>();
    let mut percent_map = BTreeMap::new();
    for (lang, val) in langs_map {
        let percent = (val / sum_of_components) * 100.0;
        percent_map.insert(lang, percent);
    }

    // Print most used languages
    println!("Most used languages:");
    let mut percents_sorted: Vec<_> = percent_map.into_iter().collect();
    percents_sorted.sort_by_key(|(_, v)| (v * 1000.0) as u32);
    percents_sorted.reverse();
    for (lang, percent) in percents_sorted.into_iter() {
        println!("{lang}: {percent}%");
    }

    // Print total stars
    let total_stars: f32 = repos_info
        .iter()
        .map(|info| {
            info.stars as f32
                * if connection.weighted {
                    info.ratio_of_commits_from_user
                } else {
                    1.0
                }
        })
        .sum();
    println!("Total stars (weighted depending on args): {total_stars}");

    Ok(())
}

#[derive(Deserialize, Debug)]
struct LOCData {
    language: String,
    #[serde(rename(deserialize = "linesOfCode"))]
    lines_of_code: u32,
}

#[derive(Deserialize, Debug)]
struct UserData {
    pub organizations_url: String,
    pub repos_url: String,
}

#[derive(Deserialize, Debug)]
struct RepoData {
    pub stargazers_count: u32,
    pub contributors_url: String,
    pub full_name: String,
}

#[derive(Deserialize, Debug)]
struct ContributorData {
    login: String,
    contributions: u32,
}

#[derive(Deserialize, Debug)]
struct OrgData {
    repos_url: String,
}
