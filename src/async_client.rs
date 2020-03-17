use futures::prelude::*;
use futures::{
    future::{try_join_all, BoxFuture},
    try_join,
};
use log::trace;
use reqwest::{header, StatusCode, Url};
use serde::de::DeserializeOwned;

use super::Error;
use crate::types::*;

pub type Result<T> = std::result::Result<T, Error>;

/// Asynchronous client for the crates.io API.
#[derive(Clone)]
pub struct Client {
    client: reqwest::Client,
    base_url: Url,
}

impl Client {
    /// Instantiate a new client.
    ///
    /// This will fail if the underlying http client could not be created.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: Url::parse("https://crates.io/api/v1/").unwrap(),
        }
    }

    pub fn with_user_agent(user_agent: &str) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_str(user_agent).unwrap(),
        );
        Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .unwrap(),
            base_url: Url::parse("https://crates.io/api/v1/").unwrap(),
        }
    }

    async fn get<T: DeserializeOwned>(&self, url: &Url) -> Result<T> {
        trace!("GET {}", url);

        self.client
            .get(url.clone())
            .send()
            .await
            .map_err(Error::from)
            .and_then(|res| {
                if res.status() == StatusCode::NOT_FOUND {
                    return Err(Error::NotFound);
                }
                let res = res.error_for_status()?;
                Ok(res)
            })?
            .json::<T>()
            .await
            .map_err(Error::from)
    }

    /// Retrieve a summary containing crates.io wide information.
    pub async fn summary(&self) -> Result<Summary> {
        let url = self.base_url.join("summary").unwrap();
        self.get(&url).await
    }

    /// Retrieve information of a crate.
    ///
    /// If you require detailed information, consider using [full_crate]().
    pub async fn get_crate(&self, name: &str) -> Result<CrateResponse> {
        let url = self.base_url.join("crates/").unwrap().join(name).unwrap();
        self.get(&url).await
    }

    /// Retrieve download stats for a crate.
    pub async fn crate_downloads(&self, name: &str) -> Result<Downloads> {
        let url = self
            .base_url
            .join(&format!("crates/{}/downloads", name))
            .unwrap();
        self.get(&url).await
    }

    /// Retrieve the owners of a crate.
    pub async fn crate_owners(&self, name: &str) -> Result<Vec<User>> {
        let url = self
            .base_url
            .join(&format!("crates/{}/owners", name))
            .unwrap();
        self.get::<Owners>(&url).await.map(|data| data.users)
    }

    /// Load all reverse dependencies of a crate.
    ///
    /// Note: Since the reverse dependency endpoint requires pagination, this
    /// will result in multiple requests if the crate has more than 100 reverse
    /// dependencies.
    pub async fn crate_reverse_dependencies(&self, name: &str) -> Result<ReverseDependencies> {
        fn fetch_page(
            c: Client,
            name: String,
            mut tidy_rdeps: ReverseDependencies,
            page: u64,
        ) -> BoxFuture<'static, Result<ReverseDependencies>> {
            let url = c
                .base_url
                .join(&format!(
                    "crates/{0}/reverse_dependencies?per_page=100&page={1}",
                    name, page
                ))
                .unwrap();

            async move {
                let rdeps = c.get::<ReverseDependenciesAsReceived>(&url).await?;
                tidy_rdeps.from_received(&rdeps);

                if !rdeps.dependencies.is_empty() {
                    tidy_rdeps.meta = rdeps.meta;
                    fetch_page(c, name, tidy_rdeps, page + 1).await
                } else {
                    Ok(tidy_rdeps)
                }
            }
            .boxed()
        }

        fetch_page(
            self.clone(),
            name.to_string(),
            ReverseDependencies {
                dependencies: Vec::new(),
                meta: Meta { total: 0 },
            },
            1,
        )
        .await
    }

    /// Retrieve the authors for a crate version.
    pub async fn crate_authors(&self, name: &str, version: &str) -> Result<Authors> {
        let url = self
            .base_url
            .join(&format!("crates/{}/{}/authors", name, version))
            .unwrap();
        self.get::<AuthorsResponse>(&url).await.map(|res| Authors {
            names: res.meta.names,
            users: res.users,
        })
    }

    /// Retrieve the dependencies of a crate version.
    pub async fn crate_dependencies(&self, name: &str, version: &str) -> Result<Vec<Dependency>> {
        let url = self
            .base_url
            .join(&format!("crates/{}/{}/dependencies", name, version))
            .unwrap();
        self.get::<Dependencies>(&url)
            .await
            .map(|res| res.dependencies)
    }

    async fn full_version(&self, version: Version) -> Result<FullVersion> {
        let authors_fut = self.crate_authors(&version.crate_name, &version.num);
        let deps_fut = self.crate_dependencies(&version.crate_name, &version.num);

        try_join!(authors_fut, deps_fut).map(|(authors, deps)| FullVersion {
            created_at: version.created_at,
            updated_at: version.updated_at,
            dl_path: version.dl_path,
            downloads: version.downloads,
            features: version.features,
            id: version.id,
            num: version.num,
            yanked: version.yanked,
            license: version.license,
            links: version.links,
            readme_path: version.readme_path,

            author_names: authors.names,
            authors: authors.users,
            dependencies: deps,
        })
    }

    /// Retrieve all available information for a crate, including download
    /// stats,  owners and reverse dependencies.
    ///
    /// The `all_versions` argument controls the retrieval of detailed version
    /// information.
    /// If false, only the data for the latest version will be fetched, if true,
    /// detailed information for all versions will be available.
    /// Note: Each version requires two extra requests.
    pub async fn full_crate(&self, name: &str, all_versions: bool) -> Result<FullCrate> {
        let c = self.clone();
        let krate = self.get_crate(name).await?;

        let versions = if !all_versions {
            c.full_version(krate.versions[0].clone())
                .await
                .map(|v| vec![v])
        } else {
            try_join_all(
                krate
                    .versions
                    .clone()
                    .into_iter()
                    .map(|v| c.full_version(v)),
            )
            .await
        }?;

        let dls_fut = self.crate_downloads(name);
        let owners_fut = self.crate_owners(name);
        let reverse_dependencies_fut = self.crate_reverse_dependencies(name);

        try_join!(dls_fut, owners_fut, reverse_dependencies_fut).map(
            |(dls, owners, reverse_dependencies)| {
                let data = krate.crate_data;
                FullCrate {
                    id: data.id,
                    name: data.name,
                    description: data.description,
                    license: krate.versions[0].license.clone(),
                    documentation: data.documentation,
                    homepage: data.homepage,
                    repository: data.repository,
                    total_downloads: data.downloads,
                    max_version: data.max_version,
                    created_at: data.created_at,
                    updated_at: data.updated_at,

                    categories: krate.categories,
                    keywords: krate.keywords,
                    downloads: dls,
                    owners,
                    reverse_dependencies,
                    versions,
                }
            },
        )
    }

    /// Retrieve a page of crates, optionally constrained by a query.
    ///
    /// If you want to get all results without worrying about paging,
    /// use [all_crates]().
    ///
    /// ```
    pub async fn crates(&self, spec: ListOptions) -> Result<CratesResponse> {
        let mut url = self.base_url.join("crates").unwrap();
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("page", &spec.page.to_string());
            q.append_pair("per_page", &spec.per_page.to_string());
            q.append_pair("sort", spec.sort.to_str());
            if let Some(query) = spec.query {
                q.append_pair("q", &query);
            }
        }
        self.get(&url).await
    }
}

/// Retrieve all crates, optionally constrained by a query.
///
/// Note: This method fetches all pages of the result.
/// This can result in a lot queries (100 results per query).
/*
pub fn all_crates(&self, query: Option<String>) -> impl Stream<Item = Crate> {
    let opts = ListOptions {
        query,
        sort: Sort::Alphabetical,
        per_page: 100,
        page: 1,
    };

    let c = self.clone();
    self.crates(opts.clone())
        .and_then(move |res| {
            let pages = (res.meta.total as f64 / 100.0).ceil() as u64;
            let streams_futures = (1..pages)
                .map(|page| {
                    let opts = ListOptions {
                        page,
                        ..opts.clone()
                    };
                    c.crates(opts)
                        .and_then(|res| future::ok(stream::iter_ok(res.crates)))
                })
                .collect::<Vec<_>>();
            let stream = stream::futures_ordered(streams_futures).flatten();
            future::ok(stream)
        })
        .flatten_stream()
}
*/

/// Retrieve all crates with all available extra information.
///
/// Note: This method fetches not only all crates, but does multiple requests for each crate
/// to retrieve extra information.
///
/// This can result in A LOT of queries.
/*
pub fn all_crates_full(
    &self,
    query: Option<String>,
    all_versions: bool,
) -> impl Stream<Item = FullCrate> {
    let c = self.clone();
    self.all_crates(query)
        .and_then(move |cr| c.full_crate(&cr.name, all_versions))
}
*/

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn test_client() {
        let client = Client::with_user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_3) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/80.0.3987.132 Safari/537.36");
        let summary = client.summary().await.unwrap();
        assert!(summary.most_downloaded.len() > 0);

        // for item in &summary.most_downloaded[0..3] {
        //     let _ = client.full_crate(&item.name, false).await.unwrap();
        // }

        // let crates = rt
        //     .block_on(client.all_crates(None).take(3).collect())
        //     .unwrap();
        // println!("{:?}", crates);
    }
}
