use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::api::types::{Interval, Period};
use crate::curl_impersonate;
use crate::error::IdxError;
use crate::runtime;

use super::raw_types::{ChartResponse, QuoteSummaryResponse};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const BASE_URL: &str = "https://query2.finance.yahoo.com";
const COOKIE_FETCH_URL: &str = "https://fc.yahoo.com";
const CRUMB_FETCH_URL: &str = "https://query1.finance.yahoo.com/v1/test/getcrumb";

pub(super) struct YahooClient {
    agent: ureq::Agent,
    crumb: Mutex<Option<String>>,
    /// One jar per client, paired with its crumb: clients in the same process
    /// (concurrent MCP calls) must not overwrite each other's cookies.
    cookie_jar: PathBuf,
    /// The jar's private directory, created on first use.
    cookie_dir: PathBuf,
    cookie_dir_ready: OnceLock<Result<(), String>>,
}

impl YahooClient {
    pub(super) fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_recv_body(Some(Duration::from_secs(10)))
            // Bounds the wait for response headers too, which nothing else does.
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();

        // A fresh, unguessable directory created with create_dir (which fails
        // if anything already exists there) and mode 0700: a predictable file
        // in the shared temp dir could be pre-planted as a symlink for curl
        // to overwrite, or read by other users.
        static NEXT_JAR: AtomicU64 = AtomicU64::new(0);
        let cookie_dir = std::env::temp_dir().join(format!(
            "idx_yf_{}_{}_{:016x}",
            std::process::id(),
            NEXT_JAR.fetch_add(1, Ordering::Relaxed),
            fastrand::u64(..)
        ));
        Self {
            agent,
            crumb: Mutex::new(None),
            cookie_jar: cookie_dir.join("cookies.txt"),
            cookie_dir,
            cookie_dir_ready: OnceLock::new(),
        }
    }

    fn ensure_cookie_dir(&self) -> Result<(), IdxError> {
        self.cookie_dir_ready
            .get_or_init(|| {
                let mut builder = std::fs::DirBuilder::new();
                #[cfg(unix)]
                std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
                builder.create(&self.cookie_dir).map_err(|e| {
                    format!(
                        "failed to create Yahoo cookie directory {}: {e}",
                        self.cookie_dir.display()
                    )
                })
            })
            .clone()
            .map_err(IdxError::Io)
    }

    fn chart_url(symbol: &str, period: &Period, interval: &Interval) -> String {
        format!(
            "{BASE_URL}/v8/finance/chart/{symbol}?range={}&interval={}",
            period.as_str(),
            interval.as_str()
        )
    }

    fn quote_summary_url(symbol: &str, crumb: &str) -> String {
        format!(
            "{BASE_URL}/v10/finance/quoteSummary/{symbol}?modules=summaryDetail,defaultKeyStatistics,financialData,assetProfile,incomeStatementHistory&crumb={}",
            percent_encode(crumb)
        )
    }

    pub(super) fn parse_crumb_body(raw: &str) -> Result<String, IdxError> {
        let crumb = raw.trim();
        if crumb.is_empty() {
            return Err(IdxError::Http("received empty Yahoo crumb".to_string()));
        }

        let normalized = crumb.to_ascii_lowercase();
        if normalized.contains("<html>") || normalized.contains("<!doctype html") {
            return Err(IdxError::Http(
                "received HTML instead of a Yahoo crumb".to_string(),
            ));
        }
        if normalized.contains("too many requests") {
            return Err(IdxError::Http(
                "Yahoo crumb request was rate limited".to_string(),
            ));
        }
        // A crumb is a short token; anything else (a JSON error body, an
        // "Unauthorized" page) would otherwise be sent as one.
        if crumb.len() > 64
            || !crumb.chars().all(|c| c.is_ascii_graphic())
            || crumb.contains(['{', '<'])
        {
            return Err(IdxError::Http(
                "received an error body instead of a Yahoo crumb".to_string(),
            ));
        }

        Ok(crumb.to_string())
    }

    fn cookie_header_from_jar(path: &Path) -> Result<String, IdxError> {
        let jar = std::fs::read_to_string(path).map_err(|e| {
            IdxError::Http(format!(
                "failed to read Yahoo cookie jar {}: {e}",
                path.display()
            ))
        })?;

        let cookies: Vec<String> = jar
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return None;
                }

                let candidate = trimmed.strip_prefix("#HttpOnly_").unwrap_or(trimmed);
                if candidate.starts_with('#') {
                    return None;
                }

                let fields: Vec<_> = candidate.split('\t').collect();
                if fields.len() < 7 {
                    return None;
                }

                let name = fields[5].trim();
                let value = fields[6].trim();
                if name.is_empty() {
                    return None;
                }

                Some(format!("{name}={value}"))
            })
            .collect();

        if cookies.is_empty() {
            return Err(IdxError::Http(format!(
                "Yahoo cookie jar {} did not contain any cookies",
                path.display()
            )));
        }

        Ok(cookies.join("; "))
    }

    fn fetch_crumb_via_curl(&self) -> Result<String, IdxError> {
        let binary = curl_impersonate::chrome_curl_binary()?;

        self.ensure_cookie_dir()?;
        let cookie_jar = &self.cookie_jar;
        let cookie_jar_str = cookie_jar.to_str().ok_or_else(|| {
            IdxError::Io(format!(
                "failed to encode Yahoo cookie jar path {}",
                cookie_jar.display()
            ))
        })?;

        // Step 1: fetch fc.yahoo.com to set A3 cookie (returns 404 but writes cookie jar).
        // We allow non-zero exit here since 404 still writes the cookie.
        let _ = Command::new(binary)
            .args(curl_impersonate::SAFETY_ARGS)
            .args(["--max-time", "30", "--silent", "--cookie-jar"])
            .args([cookie_jar_str, COOKIE_FETCH_URL])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();

        // Step 2: fetch crumb with cookie jar (Chrome TLS fingerprint + A3 cookie).
        let output = curl_impersonate::run(
            "Yahoo crumb fetch",
            &[
                "--silent",
                "--max-time",
                "30",
                "--cookie",
                cookie_jar_str,
                CRUMB_FETCH_URL,
            ],
        )?;

        let body = String::from_utf8_lossy(&output.stdout);
        Self::parse_crumb_body(&body)
    }

    fn get_or_init_crumb(&self) -> Result<String, IdxError> {
        let mut guard = self
            .crumb
            .lock()
            .map_err(|e| IdxError::Io(format!("crumb lock poisoned: {e}")))?;

        if let Some(crumb) = guard.as_ref() {
            return Ok(crumb.clone());
        }

        let crumb = self.fetch_crumb_via_curl()?;
        *guard = Some(crumb.clone());
        Ok(crumb)
    }

    fn clear_crumb(&self) -> Result<(), IdxError> {
        let mut guard = self
            .crumb
            .lock()
            .map_err(|e| IdxError::Io(format!("crumb lock poisoned: {e}")))?;
        *guard = None;
        Ok(())
    }

    fn retry_rate_limited<T, F>(&self, request: F) -> Result<Result<T, ureq::Error>, IdxError>
    where
        F: FnMut() -> Result<T, ureq::Error>,
    {
        Self::retry_rate_limited_with(request, std::thread::sleep, jitter)
    }

    fn retry_rate_limited_with<T, F, S, J>(
        mut request: F,
        mut sleeper: S,
        mut jitter_fn: J,
    ) -> Result<Result<T, ureq::Error>, IdxError>
    where
        F: FnMut() -> Result<T, ureq::Error>,
        S: FnMut(Duration),
        J: FnMut() -> Duration,
    {
        let mut wait = Duration::from_millis(250);

        for attempt in 0..3 {
            match request() {
                Err(ureq::Error::StatusCode(429)) => {
                    if attempt < 2 {
                        sleeper(wait + jitter_fn());
                        wait *= 2;
                        continue;
                    }
                    return Err(IdxError::RateLimited);
                }
                other => return Ok(other),
            }
        }

        Err(IdxError::RateLimited)
    }

    pub(super) fn fetch_chart(
        &self,
        symbol: &str,
        period: &Period,
        interval: &Interval,
    ) -> Result<ChartResponse, IdxError> {
        let url = Self::chart_url(symbol, period, interval);
        let response = self
            .retry_rate_limited(|| self.agent.get(&url).header("User-Agent", USER_AGENT).call())?;

        match response {
            Ok(ok) => ok
                .into_body()
                .read_json::<ChartResponse>()
                .map_err(|e| IdxError::ParseError(e.to_string())),
            Err(ureq::Error::StatusCode(404)) => Err(IdxError::SymbolNotFound(symbol.to_string())),
            Err(e) => Err(IdxError::Http(e.to_string())),
        }
    }

    pub(super) fn fetch_quote_summary(
        &self,
        symbol: &str,
    ) -> Result<QuoteSummaryResponse, IdxError> {
        for auth_attempt in 0..2 {
            let crumb = self.get_or_init_crumb()?;
            let cookie_header = match Self::cookie_header_from_jar(&self.cookie_jar) {
                Ok(header) => header,
                Err(err) => {
                    runtime::warn(format!("failed to parse Yahoo cookie jar: {err}"));
                    return Err(IdxError::AuthError(format!(
                        "failed to parse Yahoo cookies: {err}"
                    )));
                }
            };
            let url = Self::quote_summary_url(symbol, &crumb);
            let response = self.retry_rate_limited(|| {
                let mut req = self.agent.get(&url).header("User-Agent", USER_AGENT);
                if !cookie_header.is_empty() {
                    req = req.header("Cookie", &cookie_header);
                }
                req.call()
            })?;

            match response {
                Ok(ok) => {
                    return ok
                        .into_body()
                        .read_json::<QuoteSummaryResponse>()
                        .map_err(|e| IdxError::ParseError(e.to_string()));
                }
                Err(ureq::Error::StatusCode(401)) => {
                    if auth_attempt == 0 {
                        self.clear_crumb()?;
                        continue;
                    }
                    return Err(IdxError::Http(
                        "yahoo quoteSummary returned unauthorized (401)".to_string(),
                    ));
                }
                Err(ureq::Error::StatusCode(404)) => {
                    return Err(IdxError::SymbolNotFound(symbol.to_string()));
                }
                Err(e) => return Err(IdxError::Http(e.to_string())),
            }
        }

        Err(IdxError::RateLimited)
    }
}

fn jitter() -> Duration {
    Duration::from_millis(fastrand::u64(0..100))
}

/// Query-string encoding: crumbs can hold `/`, `+` or `=`.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl Drop for YahooClient {
    fn drop(&mut self) {
        if matches!(self.cookie_dir_ready.get(), Some(Ok(()))) {
            let _ = std::fs::remove_dir_all(&self.cookie_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::YahooClient;
    use std::time::Duration;

    #[test]
    fn parses_crumb_body_trimmed() {
        let crumb = YahooClient::parse_crumb_body("  abc123xyz\n").expect("crumb should parse");
        assert_eq!(crumb, "abc123xyz");

        let empty = YahooClient::parse_crumb_body(" \n").expect_err("empty crumb must fail");
        assert!(matches!(empty, crate::error::IdxError::Http(_)));

        let html = YahooClient::parse_crumb_body("<html>blocked</html>")
            .expect_err("html crumb must fail");
        assert!(matches!(html, crate::error::IdxError::Http(_)));

        let rate_limited = YahooClient::parse_crumb_body("Too Many Requests")
            .expect_err("rate limited crumb must fail");
        assert!(matches!(rate_limited, crate::error::IdxError::Http(_)));

        for body in [r#"{"finance":{"error":"Unauthorized"}}"#, "Invalid Cookie"] {
            assert!(YahooClient::parse_crumb_body(body).is_err(), "{body}");
        }
    }

    #[test]
    fn crumb_is_percent_encoded_in_the_url() {
        let url = YahooClient::quote_summary_url("BBCA.JK", "a/b+c=");
        assert!(url.ends_with("&crumb=a%2Fb%2Bc%3D"), "{url}");
    }

    #[test]
    fn retry_rate_limited_succeeds_after_retry() {
        let mut attempts = 0;
        let mut sleeps = Vec::new();

        let result = YahooClient::retry_rate_limited_with(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(ureq::Error::StatusCode(429))
                } else {
                    Ok("ok")
                }
            },
            |duration| sleeps.push(duration),
            || Duration::ZERO,
        )
        .expect("retry helper should not fail")
        .expect("third attempt should succeed");

        assert_eq!(result, "ok");
        assert_eq!(attempts, 3);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(250), Duration::from_millis(500)]
        );
    }

    #[test]
    fn retry_rate_limited_returns_rate_limited_after_exhaustion() {
        let mut attempts = 0;
        let mut sleeps = Vec::new();

        let err = YahooClient::retry_rate_limited_with(
            || {
                attempts += 1;
                Err::<(), ureq::Error>(ureq::Error::StatusCode(429))
            },
            |duration| sleeps.push(duration),
            || Duration::ZERO,
        )
        .expect_err("rate-limited helper should fail after three attempts");

        assert!(matches!(err, crate::error::IdxError::RateLimited));
        assert_eq!(attempts, 3);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(250), Duration::from_millis(500)]
        );
    }
}
