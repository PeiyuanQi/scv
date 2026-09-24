//! Web tools: `web_fetch` and the configured-backend `web_search`.
//!
//! `web_fetch` refuses loopback, private, link-local, and other non-public
//! addresses unless the user allows them. Host names are resolved once by a
//! checking resolver whose addresses are the only ones the client connects
//! to, so a name cannot be rebound to a local address between the check and
//! the connection. IP-literal URLs, including redirect targets, are checked
//! before any request.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::{
    Url,
    dns::{Addrs, Name, Resolve, Resolving},
    redirect,
};
use scv_core::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolRisk, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args::{bounded, parse_args};

const USER_AGENT: &str = concat!(
    "scv/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/PeiyuanQi/scv)"
);
const MAX_URL_BYTES: usize = 4096;
const MAX_QUERY_BYTES: usize = 512;
const MAX_SEARCH_RESPONSE_BYTES: usize = 1024 * 1024;
const HTML_WIDTH: usize = 120;

/// Web tool settings resolved from the user's configuration.
#[derive(Debug, Clone)]
pub struct WebToolsConfig {
    pub fetch_max_bytes: usize,
    pub fetch_timeout: Duration,
    pub max_redirects: usize,
    /// HTTPS hosts fetched without approval. `*.example.com` matches
    /// subdomains of `example.com` but not the domain itself.
    pub auto_approve_domains: Vec<String>,
    pub allow_private_addresses: bool,
    pub search: Option<SearchBackend>,
    pub max_search_results: usize,
    pub output_limit: usize,
}

/// A search service that SCV queries itself. Provider-hosted search is
/// configured on the provider instead and needs no SCV tool.
#[derive(Clone)]
pub enum SearchBackend {
    Searxng { url: String },
    Brave { url: String, api_key: String },
}

impl std::fmt::Debug for SearchBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Searxng { url } => formatter.debug_struct("Searxng").field("url", url).finish(),
            Self::Brave { url, .. } => formatter
                .debug_struct("Brave")
                .field("url", url)
                .field("api_key", &"[REDACTED]")
                .finish(),
        }
    }
}

/// Registers `web_fetch`, and `web_search` when a search backend is configured.
pub fn register(registry: &mut ToolRegistry, config: WebToolsConfig) -> Result<(), ToolError> {
    let config = Arc::new(config);
    let allow_private = config.allow_private_addresses;
    registry.register(Arc::new(WebFetchTool {
        config: Arc::clone(&config),
        address_allowed: Arc::new(move |address: SocketAddr| {
            allow_private || is_public(address.ip())
        }),
    }))?;
    if let Some(backend) = config.search.clone() {
        registry.register(Arc::new(WebSearchTool {
            backend,
            config: Arc::clone(&config),
        }))?;
    }
    Ok(())
}

/// Decides whether the client may connect to an address. Production uses
/// [`is_public`]; tests substitute a port-aware check for loopback servers.
type AddressCheck = Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;

struct WebFetchTool {
    config: Arc<WebToolsConfig>,
    address_allowed: AddressCheck,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
    #[serde(default)]
    offset: Option<usize>,
}

impl WebFetchTool {
    fn parse_url(&self, value: &str) -> Result<Url, ToolError> {
        if value.len() > MAX_URL_BYTES {
            return Err(ToolError(format!("url exceeds {MAX_URL_BYTES} bytes")));
        }
        let url =
            Url::parse(value.trim()).map_err(|error| ToolError(format!("invalid url: {error}")))?;
        check_url_shape(&url)?;
        Ok(url)
    }

    fn auto_approved(&self, url: &Url) -> bool {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| domain_listed(&self.config.auto_approve_domains, host))
    }
}

/// Only plain HTTP(S) URLs with a host and no embedded credentials.
fn check_url_shape(url: &Url) -> Result<(), ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError(format!(
            "web_fetch supports only http and https URLs, not {}",
            url.scheme()
        )));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(ToolError("url has no host".into()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError(
            "urls with embedded credentials are not allowed".into(),
        ));
    }
    Ok(())
}

/// The literal IP of a URL host, if it is one.
fn host_ip(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

fn check_literal(url: &Url, allowed: &AddressCheck) -> Result<(), String> {
    if let Some(ip) = host_ip(url) {
        let port = url.port_or_known_default().unwrap_or(0);
        if !allowed(SocketAddr::new(ip, port)) {
            return Err(format!(
                "{ip} is a loopback, private, or otherwise non-public address"
            ));
        }
    }
    Ok(())
}

/// Case-insensitive host match against the allowlist.
pub fn domain_listed(domains: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    domains.iter().any(|entry| {
        let entry = entry.trim_end_matches('.').to_ascii_lowercase();
        match entry.strip_prefix("*.") {
            Some(parent) => host
                .strip_suffix(parent)
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
            None => host == entry,
        }
    })
}

/// Whether an address is a routable public one: not loopback, private,
/// link-local, shared (CGNAT), multicast, documentation, reserved, or an IPv6
/// form that embeds such an IPv4 address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    // IPv4-compatible (deprecated) and NAT64 addresses embed an IPv4 address
    // in their low 32 bits.
    let embedded = Ipv4Addr::from(ip.to_bits() as u32);
    if segments[..6] == [0; 6] || segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return !ip.is_unspecified() && !ip.is_loopback() && is_public_v4(embedded);
    }
    // 6to4 embeds its IPv4 address in bits 16..48.
    if segments[0] == 0x2002 {
        let v4 = Ipv4Addr::new(
            (segments[1] >> 8) as u8,
            segments[1] as u8,
            (segments[2] >> 8) as u8,
            segments[2] as u8,
        );
        return is_public_v4(v4);
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00 // unique local
        || (segments[0] & 0xffc0) == 0xfe80 // link-local
        || (segments[0] & 0xffc0) == 0xfec0 // site-local
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || (segments[0] == 0x2001 && segments[1] == 0)) // Teredo
}

/// Resolves a name and fails if any of its addresses is refused, so the
/// client only ever connects to addresses that passed the check.
struct CheckedResolver {
    allowed: AddressCheck,
}

impl Resolve for CheckedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allowed = Arc::clone(&self.allowed);
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addresses = resolve_checked(&host, &allowed).await?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

async fn resolve_checked(
    host: &str,
    allowed: &AddressCheck,
) -> Result<Vec<SocketAddr>, Box<dyn std::error::Error + Send + Sync>> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 0)).await?.collect();
    check_resolved(host, &addresses, allowed)?;
    Ok(addresses)
}

/// Every resolved address must pass; one refused address refuses the name.
fn check_resolved(
    host: &str,
    addresses: &[SocketAddr],
    allowed: &AddressCheck,
) -> Result<(), String> {
    if addresses.is_empty() {
        return Err(format!("{host} did not resolve to any address"));
    }
    if let Some(refused) = addresses.iter().find(|address| !allowed(**address)) {
        return Err(format!(
            "{host} resolves to {}, a loopback, private, or otherwise non-public address",
            refused.ip()
        ));
    }
    Ok(())
}

enum Body {
    Html,
    Text,
}

/// Classifies a response by its media type, sniffing only when none is given.
fn classify(content_type: Option<&str>, bytes: &[u8]) -> Result<Body, String> {
    let Some(media) = content_type.map(|value| {
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }) else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
        return if head.contains("<html") || head.contains("<!doctype html") {
            Ok(Body::Html)
        } else if std::str::from_utf8(bytes).is_ok()
            || std::str::from_utf8(&bytes[..bytes.len().saturating_sub(4)]).is_ok()
        {
            Ok(Body::Text)
        } else {
            Err("the response has no content type and is not text".into())
        };
    };
    if media == "text/html" || media == "application/xhtml+xml" {
        return Ok(Body::Html);
    }
    let textual = media.starts_with("text/")
        || media.ends_with("+json")
        || media.ends_with("+xml")
        || matches!(
            media.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/ecmascript"
                | "application/x-javascript"
                | "application/toml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/x-ndjson"
                | "application/sql"
                | "application/graphql"
        );
    if textual {
        Ok(Body::Text)
    } else {
        Err(format!(
            "web_fetch returns text only; {media} is not a text content type"
        ))
    }
}

/// One page of `text` starting at character `offset`, within `budget` bytes.
/// Returns the page and the offset of the next page, if any.
fn page(text: &str, offset: usize, budget: usize) -> (String, Option<usize>) {
    let mut output = String::new();
    for (taken, character) in text.chars().skip(offset).enumerate() {
        if output.len() + character.len_utf8() > budget {
            return (output, Some(offset + taken));
        }
        output.push(character);
    }
    (output, None)
}

fn error_chain(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: format!(
                "Fetch a public web page or HTTP API with a GET request and return it as readable text \
                 (HTML is converted to text; JSON and plain text pass through). Use it to read \
                 documentation, release notes, issues, or a URL the user gave, and to open results \
                 from web search. Long pages are returned in parts: call again with the reported \
                 offset. Only public addresses are reachable. HTTPS pages on {} are fetched without \
                 approval; other hosts need approval because the URL is sent to that site.",
                if self.config.auto_approve_domains.is_empty() {
                    "no hosts".to_owned()
                } else {
                    self.config.auto_approve_domains.join(", ")
                }
            ),
            parameters: json!({
                "type":"object",
                "properties":{
                    "url":{"type":"string","description":"Absolute http or https URL"},
                    "offset":{"type":"integer","minimum":0,"description":"Character offset of the part to return, from a previous call"}
                },
                "required":["url"],
                "additionalProperties":false
            }),
        }
    }

    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: FetchArgs = parse_args(arguments)?;
        let url = self.parse_url(&args.url)?;
        Ok(if self.auto_approved(&url) {
            ToolRisk::ReadOnly
        } else {
            ToolRisk::Network
        })
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: FetchArgs = parse_args(arguments)?;
        let url = self.parse_url(&args.url)?;
        Ok(format!(
            "Fetch {} with an HTTP GET (no cookies or credentials). The full URL is sent to {}.",
            bounded(url.as_str(), 2000),
            url.host_str().unwrap_or("the host")
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: FetchArgs = parse_args(&arguments)?;
        let url = self.parse_url(&args.url)?;
        check_literal(&url, &self.address_allowed).map_err(ToolError)?;
        let auto_approved = self.auto_approved(&url);
        let max_redirects = self.config.max_redirects;
        let domains = self.config.auto_approve_domains.clone();
        let allowed = Arc::clone(&self.address_allowed);
        let policy = redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > max_redirects {
                return attempt.error(format!("stopped after {max_redirects} redirects"));
            }
            let next = attempt.url().clone();
            if let Err(error) = check_url_shape(&next) {
                return attempt.error(error.0);
            }
            if let Err(error) = check_literal(&next, &allowed) {
                return attempt.error(error);
            }
            if auto_approved
                && !(next.scheme() == "https"
                    && next
                        .host_str()
                        .is_some_and(|host| domain_listed(&domains, host)))
            {
                return attempt.error(format!(
                    "redirected to {next}, outside the auto-approved hosts; call web_fetch with that URL to request approval"
                ));
            }
            attempt.follow()
        });
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(self.config.fetch_timeout)
            .connect_timeout(self.config.fetch_timeout.min(Duration::from_secs(10)))
            .redirect(policy)
            .referer(false)
            .no_proxy()
            .dns_resolver(Arc::new(CheckedResolver {
                allowed: Arc::clone(&self.address_allowed),
            }))
            .build()
            .map_err(|error| ToolError(format!("create HTTP client: {error}")))?;
        let request = client.get(url.clone()).header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,text/plain;q=0.9,application/json;q=0.9,*/*;q=0.5",
        );
        let response = tokio::select! {
            result = request.send() => result.map_err(|error| ToolError(format!("fetch {url}: {}", error_chain(&error))))?,
            _ = context.cancellation.cancelled() => return Err(ToolError("web fetch cancelled".into())),
        };
        let status = response.status();
        let final_url = response.url().clone();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // Refuse a declared binary type before downloading it.
        if content_type.is_some()
            && let Err(message) = classify(content_type.as_deref(), &[])
        {
            return Ok(ToolOutput::failure(format!(
                "URL: {final_url}\nStatus: {}\n{message}",
                status.as_u16()
            )));
        }
        let limit = self.config.fetch_max_bytes;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
        let mut download_truncated = false;
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = context.cancellation.cancelled() => return Err(ToolError("web fetch cancelled".into())),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk
                .map_err(|error| ToolError(format!("read {final_url}: {}", error_chain(&error))))?;
            let remaining = limit - bytes.len();
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                download_truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let kind = match classify(content_type.as_deref(), &bytes) {
            Ok(kind) => kind,
            Err(message) => {
                return Ok(ToolOutput::failure(format!(
                    "URL: {final_url}\nStatus: {}\n{message}",
                    status.as_u16()
                )));
            }
        };
        let text = match kind {
            Body::Text => String::from_utf8_lossy(&bytes).into_owned(),
            Body::Html => tokio::task::spawn_blocking(move || {
                html2text::from_read(bytes.as_slice(), HTML_WIDTH)
                    .map_err(|error| ToolError(format!("convert HTML: {error}")))
            })
            .await
            .map_err(|error| ToolError(format!("HTML conversion task failed: {error}")))??,
        };
        let offset = args.offset.unwrap_or(0);
        let total = text.chars().count();
        let header = format!(
            "URL: {final_url}\nStatus: {}\nContent-Type: {}\nCharacters: {offset}-{{end}} of {total}{}\n\n",
            status.as_u16(),
            content_type.as_deref().unwrap_or("unknown"),
            if download_truncated {
                format!(" (download stopped at {limit} bytes)")
            } else {
                String::new()
            }
        );
        let footer_reserve = 160;
        let budget = self
            .config
            .output_limit
            .saturating_sub(header.len() + footer_reserve)
            .max(1);
        let (body, next) = page(&text, offset, budget);
        let end = offset + body.chars().count();
        let mut content = header.replace("{end}", &end.to_string());
        content.push_str(&body);
        if let Some(next) = next {
            content.push_str(&format!(
                "\n\n[{} more characters; call web_fetch with offset={next} for the next part]",
                total - next
            ));
        }
        Ok(ToolOutput {
            content,
            is_error: status.is_client_error() || status.is_server_error(),
            truncated: next.is_some() || download_truncated,
        })
    }
}

struct WebSearchTool {
    backend: SearchBackend,
    config: Arc<WebToolsConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

impl WebSearchTool {
    fn validate(&self, args: &SearchArgs) -> Result<(), ToolError> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err(ToolError("query must not be empty".into()));
        }
        if query.len() > MAX_QUERY_BYTES {
            return Err(ToolError(format!("query exceeds {MAX_QUERY_BYTES} bytes")));
        }
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        match self.backend {
            SearchBackend::Searxng { .. } => "SearXNG",
            SearchBackend::Brave { .. } => "Brave Search",
        }
    }
}

#[derive(Debug, PartialEq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn parse_results(backend: &SearchBackend, body: &Value) -> Result<Vec<SearchResult>, String> {
    let (items, snippet_field) = match backend {
        SearchBackend::Searxng { .. } => (body.get("results"), "content"),
        SearchBackend::Brave { .. } => (
            body.get("web").and_then(|web| web.get("results")),
            "description",
        ),
    };
    let Some(items) = items else {
        return Ok(Vec::new());
    };
    let items = items
        .as_array()
        .ok_or_else(|| "search results are not a list".to_owned())?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let url = item.get("url")?.as_str()?.to_owned();
            let text = |field: &str| {
                item.get(field)
                    .and_then(Value::as_str)
                    .map(strip_tags)
                    .unwrap_or_default()
            };
            Some(SearchResult {
                title: text("title"),
                url,
                snippet: text(snippet_field),
            })
        })
        .collect())
}

/// Removes markup such as Brave's `<strong>` highlights from a snippet.
fn strip_tags(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut in_tag = false;
    for character in value.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_results(query: &str, results: &[SearchResult], limit: usize) -> String {
    if results.is_empty() {
        return format!("No results for {query:?}.");
    }
    let mut output = format!("Results for {query:?}:\n");
    for (index, result) in results.iter().enumerate() {
        let entry = format!(
            "\n{}. {}\n   {}\n   {}\n",
            index + 1,
            if result.title.is_empty() {
                "(untitled)"
            } else {
                &result.title
            },
            result.url,
            bounded(&result.snippet, 400)
        );
        if output.len() + entry.len() > limit {
            break;
        }
        output.push_str(&entry);
    }
    output
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: format!(
                "Search the web with {} and return result titles, URLs, and snippets. Use it for \
                 current facts, versions, documentation locations, and error messages, then open \
                 the most relevant results with web_fetch.",
                self.backend_name()
            ),
            parameters: json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string"},
                    "count":{"type":"integer","minimum":1,"maximum":self.config.max_search_results,"description":"Number of results (default and maximum shown)"}
                },
                "required":["query"],
                "additionalProperties":false
            }),
        }
    }

    // The query goes only to the search service the user configured, so it
    // cannot carry data to a host the model chooses.
    fn risk(&self, arguments: &Value) -> Result<ToolRisk, ToolError> {
        let args: SearchArgs = parse_args(arguments)?;
        self.validate(&args)?;
        Ok(ToolRisk::ReadOnly)
    }

    fn approval_summary(&self, arguments: &Value) -> Result<String, ToolError> {
        let args: SearchArgs = parse_args(arguments)?;
        self.validate(&args)?;
        Ok(format!(
            "Search {} for {:?}",
            self.backend_name(),
            bounded(args.query.trim(), 500)
        ))
    }

    async fn execute(
        &self,
        arguments: Value,
        context: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let args: SearchArgs = parse_args(&arguments)?;
        self.validate(&args)?;
        let query = args.query.trim().to_owned();
        let count = args
            .count
            .unwrap_or(self.config.max_search_results)
            .clamp(1, self.config.max_search_results);
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(self.config.fetch_timeout)
            .redirect(redirect::Policy::limited(3))
            .build()
            .map_err(|error| ToolError(format!("create HTTP client: {error}")))?;
        let request = match &self.backend {
            SearchBackend::Searxng { url } => {
                let endpoint = format!("{}/search", url.trim_end_matches('/'));
                client
                    .get(endpoint)
                    .query(&[("q", query.as_str()), ("format", "json")])
            }
            SearchBackend::Brave { url, api_key } => client
                .get(url)
                .query(&[("q", query.as_str()), ("count", &count.to_string())])
                .header(reqwest::header::ACCEPT, "application/json")
                .header("X-Subscription-Token", api_key),
        };
        let response = tokio::select! {
            result = request.send() => result.map_err(|error| ToolError(format!("{} request failed: {}", self.backend_name(), error_chain(&error))))?,
            _ = context.cancellation.cancelled() => return Err(ToolError("web search cancelled".into())),
        };
        let status = response.status();
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        loop {
            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = context.cancellation.cancelled() => return Err(ToolError("web search cancelled".into())),
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|error| {
                ToolError(format!(
                    "{} response failed: {}",
                    self.backend_name(),
                    error_chain(&error)
                ))
            })?;
            if bytes.len() + chunk.len() > MAX_SEARCH_RESPONSE_BYTES {
                return Err(ToolError(format!(
                    "{} response exceeded {MAX_SEARCH_RESPONSE_BYTES} bytes",
                    self.backend_name()
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned();
            let hint = match (&self.backend, status.as_u16()) {
                (SearchBackend::Searxng { .. }, 403) => {
                    " (enable the json format under search.formats in SearXNG's settings.yml)"
                }
                (SearchBackend::Brave { .. }, 401 | 403 | 422) => {
                    " (check the Brave Search API key)"
                }
                _ => "",
            };
            return Ok(ToolOutput::failure(format!(
                "{} returned HTTP {}{hint}: {}",
                self.backend_name(),
                status.as_u16(),
                bounded(&body, 300)
            )));
        }
        let body: Value = serde_json::from_slice(&bytes).map_err(|error| {
            ToolError(format!(
                "{} returned invalid JSON: {error}",
                self.backend_name()
            ))
        })?;
        let mut results = parse_results(&self.backend, &body).map_err(ToolError)?;
        results.truncate(count);
        Ok(ToolOutput::success(format_results(
            &query,
            &results,
            self.config.output_limit,
        )))
    }
}

#[cfg(test)]
mod tests;
