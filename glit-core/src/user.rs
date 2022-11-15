use ahash::HashMap;
use std::{sync::mpsc, thread};

use colored::Colorize;
use futures::{future::join_all, stream, StreamExt};
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

use crate::{
    config::{RepositoryConfig, UserConfig},
    repo::{Repository, RepositoryCommitData, RepositoryFactory},
    CommittedDataExtraction,
};

const NUMBER_OF_REPO_PER_PAGE: u32 = 30;

pub struct User {
    name: String,
    url: Url,
    repositories: Vec<Repository>, // Network action
}

pub struct UserFactory {
    url: Url,
    page_url: Url,
    all_branches: bool,
}

impl UserFactory {
    pub fn with_config(user_config: UserConfig) -> Self {
        let url = user_config.url;

        let page_url = format!("{}?tab=repositories&type=source", url);
        let page_url = Url::parse(&page_url).unwrap();

        let all_branches: bool = user_config.all_branches;

        UserFactory {
            url,
            page_url,
            all_branches,
        }
    }

    pub async fn repositories_count(client: &Client, url: Url) -> u32 {
        let resp = client.get(url).send().await.unwrap();
        let text = resp.text().await.unwrap();

        let parser = Html::parse_document(&text);
        let selector_repositories_count =
            Selector::parse(r#"turbo-frame > div > div > div > div > strong"#).unwrap();

        let repository_count_str = parser
            .select(&selector_repositories_count)
            .next()
            .unwrap()
            .inner_html();

        repository_count_str
            .trim()
            .replace(',', "")
            .parse::<u32>()
            .unwrap()
    }

    fn pages_count(repo_count: u32) -> u32 {
        let modulo = repo_count % NUMBER_OF_REPO_PER_PAGE;
        if modulo.eq(&0) {
            repo_count / NUMBER_OF_REPO_PER_PAGE
        } else {
            ((repo_count - modulo) / NUMBER_OF_REPO_PER_PAGE) + 1
        }
    }

    pub async fn create(self, client: &Client) -> User {
        let url = self.url;
        let page_url = self.page_url;

        let mut path_segment = url.path_segments().unwrap();
        let name = path_segment.next().unwrap().to_string();

        let repo_count = Self::repositories_count(client, page_url.clone()).await;
        let pages_count = Self::pages_count(repo_count);

        println!(
            "User {} have {} repositories to process.\nBuilding repositories urls ...",
            name.clone().blue(),
            repo_count.to_string().yellow()
        );

        let mut pages_urls = Vec::new();

        for i in 1..pages_count + 1 {
            let url = format!("{}&page={}", page_url, i);
            pages_urls.push(Url::parse(&url).unwrap());
        }

        let repositories =
            Self::fetch_repository_list(client, url.clone(), pages_urls, self.all_branches).await;

        User {
            name,
            url,
            repositories,
        }
    }

    async fn fetch_repository_list(
        client: &Client,
        base_url: Url,
        pages_urls: Vec<Url>,
        all_branches: bool,
    ) -> Vec<Repository> {
        let content = stream::iter(pages_urls)
            .map(|url| async {
                let client = client.clone();
                let base_url = base_url.clone();

                tokio::spawn(async move {
                    let client = &client.clone();

                    let resp = client.get(url).send().await.unwrap();
                    let text = resp.text().await.unwrap();

                    let parser = Html::parse_document(&text);
                    let selector_repositories_url = Selector::parse(
                        r#"turbo-frame > div > div > ul > li > div > div > h3 > a"#,
                    )
                    .unwrap();

                    let repositories = parser
                        .select(&selector_repositories_url)
                        .map(|l| {
                            let endpoint_url = l.value().attr("href").unwrap().to_string();

                            let repo_name = endpoint_url.split('/').last().unwrap();

                            let repo_url = format!("{}{}/", base_url, repo_name);

                            Url::parse(&repo_url).unwrap()
                        })
                        .collect::<Vec<Url>>();

                    repositories
                })
                .await
                .unwrap()
            })
            .buffer_unordered(8)
            .collect::<Vec<Vec<Url>>>();

        join_all(
            content
                .await
                .into_iter()
                .flatten()
                .map(|u| async {
                    let repo_config = RepositoryConfig {
                        url: u,
                        branchs: Vec::new(),
                        all_branches,
                    };

                    RepositoryFactory::with_config(repo_config).create()
                })
                .into_iter()
                .map(|x| async { x.await }),
        )
        .await
    }
}

type RepoName = String;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCommitData {
    pub repositories_data: HashMap<RepoName, RepositoryCommitData>,
}

impl CommittedDataExtraction<HashMap<RepoName, UserCommitData>> for User {
    fn committed_data(self) -> HashMap<RepoName, UserCommitData> {
        let mut handles = vec![];
        let (tx, rx) = mpsc::channel();

        for repository in self.repositories {
            let tx = mpsc::Sender::clone(&tx);

            let handle = thread::spawn(move || {
                let commited = repository.clone().committed_data();
                let user_commit_data = UserCommitData {
                    repositories_data: commited,
                };

                tx.send((repository.name, user_commit_data)).unwrap();
            });

            handles.push(handle);
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .for_each(drop);

        drop(tx);

        rx.into_iter().collect::<HashMap<String, UserCommitData>>()
    }
}
