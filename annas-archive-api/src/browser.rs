//! Lets the user pass Anna's Archive's browser check in a real Chrome window,
//! then hands the resulting cookies back so the HTTP client can reuse them.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use crate::error::Error;

/// Overrides the browser binary to launch.
const CHROME_ENV: &str = "ANNAS_ARCHIVE_CHROME";

const CHROME_PATHS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    r"C:\Program Files\Google\Chrome\Application\chrome.exe",
    r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
];

const CHROME_NAMES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
];

type DevTools = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn browser_error(e: impl std::fmt::Display) -> Error {
    Error::Browser {
        message: e.to_string(),
    }
}

fn find_chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(CHROME_ENV) {
        return Some(path.into());
    }

    let installed = CHROME_PATHS.iter().map(PathBuf::from);
    let on_path = std::env::var_os("PATH").into_iter().flat_map(|path| {
        std::env::split_paths(&path)
            .flat_map(|dir| CHROME_NAMES.iter().map(move |name| dir.join(name)))
            .collect::<Vec<_>>()
    });

    installed.chain(on_path).find(|path| path.is_file())
}

/// Open a search page on `domain` in Chrome and wait for the user to get past
/// the browser check. Returns the `(name, value)` cookies the browser ended up
/// with for that domain.
pub(crate) async fn pass_browser_check(
    domain: &str,
    timeout: Duration,
) -> Result<Vec<(String, String)>, Error> {
    let chrome = find_chrome().ok_or_else(|| {
        browser_error(format!(
            "No Chrome-based browser found - set {CHROME_ENV} to its path"
        ))
    })?;

    // Chrome only allows remote debugging on a non-default profile
    let profile = std::env::temp_dir().join("annas-archive-mcp-chrome");
    tokio::fs::create_dir_all(&profile)
        .await
        .map_err(browser_error)?;
    let port_file = profile.join("DevToolsActivePort");
    let _ = tokio::fs::remove_file(&port_file).await;

    // Dropping the child closes the window, whichever way we leave
    let _chrome = Command::new(chrome)
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(format!("https://{domain}/search?q=test"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(browser_error)?;

    tokio::time::timeout(timeout, async {
        let mut devtools = connect(&port_file).await?;
        let mut id = 0;

        while !shows_search_results(
            &call(&mut devtools, &mut id, "Target.getTargets").await?,
            domain,
        ) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let cookies = call(&mut devtools, &mut id, "Storage.getCookies").await?;
        Ok(domain_cookies(&cookies, domain))
    })
    .await
    .map_err(|_| browser_error("Timed out waiting for the browser check to be passed"))?
}

/// Connect to the DevTools endpoint Chrome advertises in its profile directory.
async fn connect(port_file: &std::path::Path) -> Result<DevTools, Error> {
    let endpoint = loop {
        if let Ok(contents) = tokio::fs::read_to_string(port_file).await
            && let Some((port, path)) = contents.trim().split_once('\n')
        {
            break format!("ws://127.0.0.1:{port}{path}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let (devtools, _) = tokio_tungstenite::connect_async(endpoint)
        .await
        .map_err(browser_error)?;
    Ok(devtools)
}

/// Send a DevTools command and wait for its result.
async fn call(devtools: &mut DevTools, id: &mut u64, method: &str) -> Result<Value, Error> {
    *id += 1;
    let command = json!({ "id": *id, "method": method }).to_string();
    devtools
        .send(Message::text(command))
        .await
        .map_err(browser_error)?;

    while let Some(message) = devtools.next().await {
        let Message::Text(text) = message.map_err(browser_error)? else {
            continue;
        };
        let mut reply: Value = serde_json::from_str(&text).map_err(browser_error)?;
        if reply["id"] == *id {
            return Ok(reply["result"].take());
        }
    }

    Err(browser_error("The browser was closed"))
}

/// Whether a tab has reached the search results on `domain`. The challenge
/// page is served from the same URL, so the page title tells them apart.
fn shows_search_results(targets: &Value, domain: &str) -> bool {
    let prefix = format!("https://{domain}/search");

    targets["targetInfos"].as_array().is_some_and(|targets| {
        targets.iter().any(|target| {
            target["type"] == "page"
                && target["url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with(&prefix))
                && target["title"]
                    .as_str()
                    .is_some_and(|title| title.contains("Search - Anna"))
        })
    })
}

/// Pick the cookies belonging to `domain` out of a `Storage.getCookies` result.
fn domain_cookies(cookies: &Value, domain: &str) -> Vec<(String, String)> {
    cookies["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|cookie| {
            cookie["domain"]
                .as_str()
                .is_some_and(|d| d.trim_start_matches('.') == domain)
        })
        .filter_map(|cookie| {
            Some((
                cookie["name"].as_str()?.to_string(),
                cookie["value"].as_str()?.to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shows_search_results() {
        let tab = |title: &str, url: &str| json!({ "targetInfos": [{ "type": "page", "title": title, "url": url }] });
        let url = "https://annas-archive.gd/search?q=test&check=1";

        assert!(shows_search_results(
            &tab("test - Search - Anna’s Archive", url),
            "annas-archive.gd"
        ));
        assert!(!shows_search_results(
            &tab("DDoS-Guard", url),
            "annas-archive.gd"
        ));
        // Title of a tab that is still loading
        assert!(!shows_search_results(
            &tab("annas-archive.gd/search?q=test", url),
            "annas-archive.gd"
        ));
        assert!(!shows_search_results(
            &tab("test - Search - Anna’s Archive", url),
            "annas-archive.gl"
        ));
    }

    #[test]
    fn test_domain_cookies() {
        let cookies = json!({ "cookies": [
            { "name": "aa_ddg_check", "value": "a", "domain": ".annas-archive.gd" },
            { "name": "ddg_last_challenge", "value": "b", "domain": "annas-archive.gd" },
            { "name": "other", "value": "c", "domain": ".example.com" },
        ]});

        assert_eq!(
            domain_cookies(&cookies, "annas-archive.gd"),
            vec![
                ("aa_ddg_check".to_string(), "a".to_string()),
                ("ddg_last_challenge".to_string(), "b".to_string()),
            ]
        );
    }
}
