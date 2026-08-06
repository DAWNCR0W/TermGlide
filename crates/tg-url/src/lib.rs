//! Address classification and search-template handling for the CLI.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

use thiserror::Error;
use url::Url;

const MAX_ADDRESS_BYTES: usize = 32 * 1024;

#[derive(Clone, PartialEq, Eq)]
pub struct BrowserUrl {
    inner: Url,
}

impl BrowserUrl {
    pub fn parse(input: &str) -> Result<Self, UrlError> {
        if input.len() > MAX_ADDRESS_BYTES {
            return Err(UrlError::TooLong);
        }
        let inner = Url::parse(input).map_err(|error| UrlError::Parse(error.to_string()))?;
        if !matches!(
            inner.scheme(),
            "http" | "https" | "about" | "data" | "file" | "blob"
        ) {
            return Err(UrlError::SchemeBlocked(inner.scheme().to_owned()));
        }
        Ok(Self { inner })
    }

    pub fn serialized(&self) -> &str {
        self.inner.as_str()
    }

    pub fn scheme(&self) -> &str {
        self.inner.scheme()
    }
}

impl fmt::Debug for BrowserUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserUrl([redacted])")
    }
}

impl fmt::Display for BrowserUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.serialized())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressInput {
    Url(Box<BrowserUrl>),
    SearchQuery(String),
}

#[derive(Debug, Error)]
pub enum UrlError {
    #[error("address exceeds the 32 KiB browser limit")]
    TooLong,
    #[error("URL parsing failed: {0}")]
    Parse(String),
    #[error("address input is empty")]
    Empty,
    #[error("search template must be HTTPS and contain exactly one '{{query}}' placeholder")]
    InvalidSearchTemplate,
    #[error("scheme '{0}' is blocked by navigation policy")]
    SchemeBlocked(String),
}

pub fn classify_address(input: &str) -> Result<AddressInput, UrlError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(UrlError::Empty);
    }
    if trimmed.len() > MAX_ADDRESS_BYTES {
        return Err(UrlError::TooLong);
    }

    if let Ok(url) = BrowserUrl::parse(trimmed) {
        return Ok(AddressInput::Url(Box::new(url)));
    }

    if looks_like_host(trimmed) {
        let scheme = if is_local_host(trimmed) {
            "http"
        } else {
            "https"
        };
        return BrowserUrl::parse(&format!("{scheme}://{trimmed}"))
            .map(|url| AddressInput::Url(Box::new(url)))
            .or_else(|_| Ok(AddressInput::SearchQuery(trimmed.to_owned())));
    }

    Ok(AddressInput::SearchQuery(trimmed.to_owned()))
}

fn looks_like_host(input: &str) -> bool {
    if input.chars().any(char::is_whitespace) {
        return false;
    }
    let authority = input.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split_once(']').map(|(host, _)| host))
        .or_else(|| authority.rsplit_once(':').map(|(host, _)| host))
        .unwrap_or(authority);
    authority.eq_ignore_ascii_case("localhost")
        || authority.starts_with("localhost:")
        || Ipv4Addr::from_str(host).is_ok()
        || Ipv6Addr::from_str(host).is_ok()
        || (host.contains('.') && !host.starts_with('.') && !host.ends_with('.'))
}

fn is_local_host(input: &str) -> bool {
    let authority = input.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .strip_prefix('[')
        .and_then(|value| value.split_once(']').map(|(host, _)| host))
        .or_else(|| authority.rsplit_once(':').map(|(host, _)| host))
        .unwrap_or(authority);
    host.eq_ignore_ascii_case("localhost")
        || Ipv4Addr::from_str(host).is_ok()
        || Ipv6Addr::from_str(host).is_ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchEngine {
    template: String,
}

impl SearchEngine {
    pub fn new(template: impl Into<String>) -> Result<Self, UrlError> {
        let template = template.into();
        if template.matches("{query}").count() != 1 {
            return Err(UrlError::InvalidSearchTemplate);
        }
        let probe = template.replace("{query}", "termglide-probe");
        let url = BrowserUrl::parse(&probe).map_err(|_| UrlError::InvalidSearchTemplate)?;
        if url.scheme() != "https" {
            return Err(UrlError::InvalidSearchTemplate);
        }
        Ok(Self { template })
    }

    pub fn resolve(&self, query: &str) -> Result<BrowserUrl, UrlError> {
        let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        BrowserUrl::parse(&self.template.replace("{query}", &encoded))
    }

    pub fn template(&self) -> &str {
        &self.template
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{AddressInput, BrowserUrl, SearchEngine, classify_address};

    #[test]
    fn classifies_urls_hosts_and_queries() -> Result<(), Box<dyn Error>> {
        assert!(matches!(
            classify_address("https://example.com")?,
            AddressInput::Url(_)
        ));
        let local = match classify_address("localhost:3000")? {
            AddressInput::Url(local) => local,
            AddressInput::SearchQuery(_) => return Err("localhost should be a URL".into()),
        };
        assert_eq!(local.serialized(), "http://localhost:3000/");
        assert!(matches!(
            classify_address("terminal browser")?,
            AddressInput::SearchQuery(_)
        ));
        Ok(())
    }

    #[test]
    fn rejects_active_content_schemes() {
        assert!(BrowserUrl::parse("javascript:alert(1)").is_err());
    }

    #[test]
    fn search_templates_are_https_and_form_encoded() -> Result<(), Box<dyn Error>> {
        let search = SearchEngine::new("https://duckduckgo.com/?q={query}")?;
        let url = search.resolve("rust terminal")?;
        assert_eq!(url.serialized(), "https://duckduckgo.com/?q=rust+terminal");
        Ok(())
    }
}
