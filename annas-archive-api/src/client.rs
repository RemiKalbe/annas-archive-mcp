use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::{Client, cookie::Jar};

use crate::browser;
use crate::error::Error;
use crate::scraper::parse_search_results;
use crate::types::{
    DownloadInfo, DownloadSource, Identifiers, IpfsInfo, ItemDetails, SearchOptions, SearchResponse,
};

/// Official mirrors, in order of preference.
const DOMAINS: &[&str] = &["annas-archive.gd", "annas-archive.gl", "annas-archive.pk"];

/// Cookie holding the logged-in session.
const ACCOUNT_COOKIE: &str = "aa_account_id2";

/// How long the user gets to pass the browser check.
const BROWSER_CHECK_TIMEOUT: Duration = Duration::from_secs(120);

pub struct AnnasArchiveClient {
    client: Client,
    api_key: Option<String>,
    cookie_jar: Arc<Jar>,
    /// Domains we have opened a session on. Cookies are scoped to a single
    /// domain, so each mirror needs its own login.
    sessions: Mutex<HashSet<&'static str>>,
    /// The mirror that last sent us to the browser check.
    challenged: Mutex<Option<&'static str>>,
}

impl AnnasArchiveClient {
    pub fn new(api_key: Option<String>) -> Self {
        let cookie_jar = Arc::new(Jar::default());

        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
            .cookie_provider(cookie_jar.clone())
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            api_key,
            cookie_jar,
            sessions: Mutex::new(HashSet::new()),
            challenged: Mutex::new(None),
        }
    }

    /// Open a session on `domain`, logging in with the secret key if we have
    /// one. Logging in sets the aa_account_id2 cookie needed for API access.
    async fn open_session(&self, domain: &'static str) -> Result<(), Error> {
        if let Some(api_key) = &self.api_key {
            self.client
                .post(format!("https://{domain}/account/"))
                .form(&[("key", api_key.as_str())])
                .send()
                .await?;
        }

        // The login page answers 200 even for a wrong key, and a seized or
        // parked domain answers 200 for anything, so ask the site whether the
        // login actually took.
        let body = self
            .client
            .get(format!("https://{domain}/dyn/up/"))
            .send()
            .await?
            .text()
            .await?;

        match parse_logged_in(&body) {
            Some(false) if self.api_key.is_some() => Err(Error::Api {
                message: "Invalid secret key".to_string(),
            }),
            Some(_) => {
                self.sessions.lock().unwrap().insert(domain);
                Ok(())
            }
            None => Err(Error::AllDomainsFailed {
                message: format!("{domain} is not serving Anna's Archive"),
            }),
        }
    }

    /// GET `path`, failing over to the next mirror on connection or server
    /// errors.
    async fn fetch(&self, path: &str) -> Result<reqwest::Response, Error> {
        let mut last_error = None;

        for domain in DOMAINS {
            if !self.sessions.lock().unwrap().contains(domain) {
                match self.open_session(domain).await {
                    Ok(()) => {}
                    // A rejected key won't be fixed by trying another domain
                    Err(e @ Error::Api { .. }) => return Err(e),
                    Err(e) => {
                        last_error = Some(e);
                        continue;
                    }
                }
            }

            match self
                .client
                .get(format!("https://{domain}{path}"))
                .send()
                .await
            {
                Ok(response) if response.status().is_server_error() => {
                    last_error = Some(Error::Http {
                        status: response.status().as_u16(),
                    });
                }
                Ok(response) if is_browser_check(&response) => {
                    *self.challenged.lock().unwrap() = Some(domain);
                    return Err(Error::BrowserCheck);
                }
                Ok(response) => return Ok(response),
                Err(e) => last_error = Some(Error::Network(e)),
            }
        }

        Err(last_error.unwrap_or(Error::AllDomainsFailed {
            message: "No domains available".to_string(),
        }))
    }

    /// Search the catalogue. Anna's Archive sends searches to a browser check
    /// unless the API key belongs to a member allowed to skip it; see
    /// [`Self::pass_browser_check`].
    pub async fn search(&self, options: SearchOptions) -> Result<SearchResponse, Error> {
        let page = options.page.unwrap_or(1);
        let query = urlencoding::encode(&options.query);
        let path = format!("/search?q={query}&page={page}");

        let response = self.fetch(&path).await?;
        if !response.status().is_success() {
            return Err(Error::Http {
                status: response.status().as_u16(),
            });
        }

        let html = response.text().await?;
        let (results, has_more) = parse_search_results(&html)?;

        Ok(SearchResponse {
            results,
            page,
            has_more,
        })
    }

    /// Open a browser window for the user to pass the browser check in, and
    /// adopt the cookies that prove it. Anna's Archive honours them for about
    /// 15 minutes.
    pub async fn pass_browser_check(&self) -> Result<(), Error> {
        let domain = self.challenged.lock().unwrap().unwrap_or(DOMAINS[0]);
        let cookies = browser::pass_browser_check(domain, BROWSER_CHECK_TIMEOUT).await?;

        let url = format!("https://{domain}/")
            .parse()
            .expect("Mirror domains are valid URLs");
        for (name, value) in cookies {
            // Keep the session we logged in with, not the browser's
            if name != ACCOUNT_COOKIE {
                let cookie = format!("{name}={value}; Domain={domain}; Path=/");
                self.cookie_jar.add_cookie_str(&cookie, &url);
            }
        }

        Ok(())
    }

    /// Get detailed metadata for an item. Requires API key (secret key).
    pub async fn get_details(&self, md5: &str) -> Result<ItemDetails, Error> {
        if self.api_key.is_none() {
            return Err(Error::MissingApiKey);
        }

        let path = format!("/db/aarecord_elasticsearch/md5:{md5}.json");

        let mut response = self.fetch(&path).await?;

        if response.status().as_u16() == 403 {
            // The session may have expired: log in again and retry once
            self.sessions.lock().unwrap().clear();
            response = self.fetch(&path).await?;
        }

        if !response.status().is_success() {
            return Err(Error::Http {
                status: response.status().as_u16(),
            });
        }

        let json_str = response.text().await?;
        parse_json_details(&json_str, md5)
    }

    pub async fn get_download_url(
        &self,
        md5: &str,
        path_index: Option<u32>,
        domain_index: Option<u32>,
    ) -> Result<DownloadInfo, Error> {
        let api_key = self.api_key.as_ref().ok_or(Error::MissingApiKey)?;

        let path_idx = path_index.unwrap_or(0);
        let domain_idx = domain_index.unwrap_or(0);

        // Try each domain for the fast download API
        let mut last_error = None;

        for domain in DOMAINS {
            let url = format!(
                "https://{domain}/dyn/api/fast_download.json?md5={md5}&path_index={path_idx}&domain_index={domain_idx}&key={api_key}"
            );

            let response = match self.client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(Error::Network(e));
                    continue;
                }
            };

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();

                // Check for common API errors
                if body.contains("no_membership") {
                    return Err(Error::Api {
                        message: "No active membership for this API key".to_string(),
                    });
                }
                if body.contains("invalid") {
                    return Err(Error::Api {
                        message: "Invalid API key".to_string(),
                    });
                }

                last_error = Some(Error::Http { status });
                continue;
            }

            #[derive(serde::Deserialize)]
            struct ApiResponse {
                download_url: Option<String>,
                error: Option<String>,
            }

            let api_response: ApiResponse = match response.json().await {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(Error::Network(e));
                    continue;
                }
            };

            if let Some(error) = api_response.error {
                return Err(Error::Api { message: error });
            }

            let download_url = api_response.download_url.ok_or(Error::Api {
                message: "No download URL in response".to_string(),
            })?;

            return Ok(DownloadInfo { download_url });
        }

        Err(last_error.unwrap_or(Error::AllDomainsFailed {
            message: "Failed to get download URL from any domain".to_string(),
        }))
    }
}

/// Parse the `/dyn/up/` login status. Returns `None` when the body is not the
/// expected JSON, which means the domain is not serving Anna's Archive.
fn parse_logged_in(body: &str) -> Option<bool> {
    let data: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(data.get("aa_logged_in")?.as_u64()? == 1)
}

/// Whether a response is the browser check's challenge page rather than an
/// answer from Anna's Archive, whose own 403s are JSON.
fn is_browser_check(response: &reqwest::Response) -> bool {
    response.status().as_u16() == 403
        && response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"))
}

/// Parse item details from the JSON API response
fn parse_json_details(json_str: &str, md5: &str) -> Result<ItemDetails, Error> {
    // The response is a JSON string that might be double-encoded
    let json_str = json_str.trim();
    let json_str = if json_str.starts_with('"') && json_str.ends_with('"') {
        // Double-encoded JSON string, parse first to get the inner JSON
        serde_json::from_str::<String>(json_str).map_err(|e| Error::Parse {
            message: format!("Failed to parse outer JSON: {e}"),
        })?
    } else {
        json_str.to_string()
    };

    let data: serde_json::Value = serde_json::from_str(&json_str).map_err(|e| Error::Parse {
        message: format!("Failed to parse JSON: {e}"),
    })?;

    // Check for error response
    if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
        return Err(Error::Api {
            message: error.to_string(),
        });
    }

    // Extract file_unified_data which contains the main metadata
    let file_data = data.get("file_unified_data").ok_or_else(|| Error::Parse {
        message: "Missing file_unified_data".to_string(),
    })?;

    let title = file_data
        .get("title_best")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();

    let author = file_data
        .get("author_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let format = file_data
        .get("extension_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase());

    let size_bytes = file_data.get("filesize_best").and_then(|v| v.as_u64());

    let size = size_bytes.map(format_filesize);

    let language = file_data
        .get("language_codes")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let publisher = file_data
        .get("publisher_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let year = file_data
        .get("year_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let description = file_data
        .get("stripped_description_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let cover_url = file_data
        .get("cover_url_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let content_type = file_data
        .get("content_type_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let original_filename = file_data
        .get("original_filename_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let added_date = file_data
        .get("added_date_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let pages = file_data
        .get("pages_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let edition = file_data
        .get("edition_varia_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let series = file_data
        .get("series_best")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Parse identifiers from identifiers_unified
    let identifiers = parse_identifiers(file_data.get("identifiers_unified"));

    // Parse categories from classifications_unified
    let categories = parse_string_list_from_object(file_data.get("classifications_unified"));

    // Parse subjects (openlib_subject, etc.)
    let subjects = parse_string_list_from_object(
        file_data
            .get("classifications_unified")
            .and_then(|c| c.get("collection")),
    )
    .or_else(|| {
        // Fallback to any subject-like classification
        file_data
            .get("classifications_unified")
            .and_then(|c| c.as_object())
            .and_then(|obj| {
                obj.iter()
                    .find(|(k, _)| k.contains("subject"))
                    .and_then(|(_, v)| {
                        v.as_array().map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                    })
            })
    });

    // Parse IPFS CIDs
    let ipfs_cids = parse_ipfs_infos(file_data.get("ipfs_infos"));

    // Parse additional data for download sources and torrent paths
    let additional = data.get("additional");

    let download_sources = parse_download_sources(additional);
    let torrent_paths = parse_torrent_paths(additional);

    Ok(ItemDetails {
        md5: md5.to_string(),
        title,
        author,
        format,
        size,
        size_bytes,
        language,
        publisher,
        year,
        description,
        cover_url,
        content_type,
        original_filename,
        added_date,
        pages,
        edition,
        series,
        identifiers,
        categories,
        subjects,
        ipfs_cids,
        download_sources,
        torrent_paths,
    })
}

/// Parse identifiers from identifiers_unified object
fn parse_identifiers(value: Option<&serde_json::Value>) -> Option<Identifiers> {
    let obj = value?.as_object()?;

    let get_string_array = |key: &str| -> Option<Vec<String>> {
        obj.get(key).and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
        })
    };

    let get_first_string = |key: &str| -> Option<String> {
        obj.get(key)
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(String::from)
    };

    let identifiers = Identifiers {
        isbn10: get_string_array("isbn10"),
        isbn13: get_string_array("isbn13"),
        doi: get_string_array("doi"),
        asin: get_string_array("asin"),
        sha1: get_first_string("sha1"),
        sha256: get_first_string("sha256"),
        crc32: get_first_string("crc32"),
        blake2b: get_first_string("blake2b"),
        open_library: get_string_array("ol"),
        google_books: get_string_array("googlebookid"),
        goodreads: get_string_array("goodreads"),
        amazon: get_string_array("amazon"),
    };

    // Only return Some if at least one field is set
    if identifiers.isbn10.is_some()
        || identifiers.isbn13.is_some()
        || identifiers.doi.is_some()
        || identifiers.asin.is_some()
        || identifiers.sha1.is_some()
        || identifiers.sha256.is_some()
        || identifiers.open_library.is_some()
        || identifiers.google_books.is_some()
    {
        Some(identifiers)
    } else {
        None
    }
}

/// Parse a list of strings from an object's values
fn parse_string_list_from_object(value: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let obj = value?.as_object()?;
    let mut result = Vec::new();

    for (key, val) in obj {
        // Skip certain keys that aren't useful categories
        if key == "collection" || key.starts_with('_') {
            continue;
        }
        if let Some(arr) = val.as_array() {
            for item in arr {
                if let Some(s) = item.as_str()
                    && !s.is_empty()
                    && !result.contains(&s.to_string())
                {
                    result.push(s.to_string());
                }
            }
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Parse IPFS info from ipfs_infos array
fn parse_ipfs_infos(value: Option<&serde_json::Value>) -> Option<Vec<IpfsInfo>> {
    let arr = value?.as_array()?;
    let infos: Vec<IpfsInfo> = arr
        .iter()
        .filter_map(|v| {
            let obj = v.as_object()?;
            let cid = obj.get("ipfs_cid")?.as_str()?.to_string();
            let from = obj
                .get("from")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(IpfsInfo { cid, from })
        })
        .collect();

    if infos.is_empty() { None } else { Some(infos) }
}

/// Parse download sources from additional data
fn parse_download_sources(additional: Option<&serde_json::Value>) -> Option<Vec<DownloadSource>> {
    let obj = additional?.as_object()?;
    let mut sources = Vec::new();

    // Check for direct download URLs
    if let Some(urls) = obj.get("download_urls").and_then(|v| v.as_array()) {
        for url in urls {
            if let Some(url_str) = url.as_str() {
                sources.push(DownloadSource {
                    name: "direct".to_string(),
                    url: url_str.to_string(),
                });
            }
        }
    }

    // Check for IPFS URLs
    if let Some(urls) = obj.get("ipfs_urls").and_then(|v| v.as_array()) {
        for url in urls {
            if let Some(url_str) = url.as_str() {
                sources.push(DownloadSource {
                    name: "ipfs".to_string(),
                    url: url_str.to_string(),
                });
            }
        }
    }

    if sources.is_empty() {
        None
    } else {
        Some(sources)
    }
}

/// Parse torrent paths from additional data
fn parse_torrent_paths(additional: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let arr = additional?.as_object()?.get("torrent_paths")?.as_array()?;

    let paths: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    if paths.is_empty() { None } else { Some(paths) }
}

fn format_filesize(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_logged_in() {
        assert_eq!(parse_logged_in(r#"{"aa_logged_in":1}"#), Some(true));
        assert_eq!(parse_logged_in(r#"{"aa_logged_in":0}"#), Some(false));
    }

    #[test]
    fn test_parse_logged_in_rejects_foreign_pages() {
        // What a parked domain serves for every path
        assert_eq!(parse_logged_in("<html><head></head></html>"), None);
        assert_eq!(parse_logged_in(r#"{"status":"ok"}"#), None);
    }
}
