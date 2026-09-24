//! Unit tests for `src/builtin/web.rs`.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use super::*;
use tokio_util::sync::CancellationToken;

fn config() -> WebToolsConfig {
    WebToolsConfig {
        fetch_max_bytes: 64 * 1024,
        fetch_timeout: Duration::from_secs(5),
        max_redirects: 3,
        auto_approve_domains: vec!["docs.rs".into(), "*.example.org".into()],
        allow_private_addresses: false,
        search: None,
        max_search_results: 5,
        output_limit: 16 * 1024,
    }
}

/// A fetch tool that may reach loopback only on the given ports, standing
/// in for "public" servers in tests.
fn fetch_tool(config: WebToolsConfig, ports: Vec<u16>) -> WebFetchTool {
    WebFetchTool {
        config: Arc::new(config),
        address_allowed: Arc::new(move |address: SocketAddr| {
            is_public(address.ip())
                || (address.ip().is_loopback() && ports.contains(&address.port()))
        }),
    }
}

fn context() -> ToolContext {
    ToolContext::new(std::env::temp_dir(), CancellationToken::new())
}

/// Serves canned HTTP responses, one per connection, and returns the
/// request heads it received.
fn serve(responses: Vec<String>) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let mut heads = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            heads.push(String::from_utf8_lossy(&head).into_owned());
            let _ = stream.write_all(response.as_bytes());
        }
        heads
    });
    (port, handle)
}

fn http(status: &str, content_type: &str, body: &str, extra: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

#[test]
fn public_address_classification() {
    for private in [
        "127.0.0.1",
        "10.1.2.3",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fc00::1",
        "fd12::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "64:ff9b::a00:1",
        "2002:7f00:1::",
        "2001:db8::1",
    ] {
        assert!(
            !is_public(private.parse().unwrap()),
            "{private} is not public"
        );
    }
    for public in [
        "1.1.1.1",
        "140.82.112.3",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
    ] {
        assert!(is_public(public.parse().unwrap()), "{public} is public");
    }
}

#[test]
fn allowlist_matches_hosts_and_wildcard_subdomains_only() {
    let domains = vec!["docs.rs".to_owned(), "*.example.org".to_owned()];
    assert!(domain_listed(&domains, "docs.rs"));
    assert!(domain_listed(&domains, "DOCS.RS."));
    assert!(!domain_listed(&domains, "evil-docs.rs"));
    assert!(!domain_listed(&domains, "sub.docs.rs"));
    assert!(domain_listed(&domains, "a.example.org"));
    assert!(domain_listed(&domains, "a.b.example.org"));
    assert!(!domain_listed(&domains, "example.org"));
    assert!(!domain_listed(&domains, "badexample.org"));
}

#[test]
fn risk_is_read_only_only_for_https_allowlisted_hosts() {
    let tool = fetch_tool(config(), Vec::new());
    let risk = |url: &str| tool.risk(&json!({"url":url}));
    assert_eq!(risk("https://docs.rs/serde").unwrap(), ToolRisk::ReadOnly);
    assert_eq!(
        risk("https://api.example.org/x").unwrap(),
        ToolRisk::ReadOnly
    );
    assert_eq!(risk("http://docs.rs/serde").unwrap(), ToolRisk::Network);
    assert_eq!(
        risk("https://attacker.test/?q=secret").unwrap(),
        ToolRisk::Network
    );
    for invalid in [
        "ftp://docs.rs/x",
        "file:///etc/passwd",
        "https://user:pass@docs.rs/",
        "not a url",
    ] {
        assert!(risk(invalid).is_err(), "{invalid} should be refused");
    }
    let summary = tool
        .approval_summary(&json!({"url":"https://attacker.test/?q=1"}))
        .unwrap();
    assert!(summary.contains("attacker.test"), "{summary}");
}

#[test]
fn resolved_names_are_refused_when_any_address_is_not_public() {
    let allowed: AddressCheck = Arc::new(|address: SocketAddr| is_public(address.ip()));
    let public: SocketAddr = "1.1.1.1:0".parse().unwrap();
    let private: SocketAddr = "10.0.0.1:0".parse().unwrap();
    assert!(check_resolved("ok.test", &[public], &allowed).is_ok());
    let error = check_resolved("mixed.test", &[public, private], &allowed).unwrap_err();
    assert!(error.contains("10.0.0.1"), "{error}");
    assert!(check_resolved("none.test", &[], &allowed).is_err());
}

#[tokio::test]
async fn html_is_converted_to_text_and_json_passes_through() {
    let (port, server) = serve(vec![
        http(
            "200 OK",
            "text/html; charset=utf-8",
            "<html><head><title>T</title><script>var hidden=1;</script></head><body><h1>Serde</h1><p>Version <b>1.0.228</b> <a href=\"https://docs.rs/serde\">docs</a></p></body></html>",
            "",
        ),
        http(
            "200 OK",
            "application/json",
            r#"{"crate":{"max_version":"1.0.228"}}"#,
            "",
        ),
    ]);
    let tool = fetch_tool(config(), vec![port]);
    let html = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/page")}),
            context(),
        )
        .await
        .unwrap();
    assert!(!html.is_error, "{}", html.content);
    assert!(html.content.contains("Serde"), "{}", html.content);
    assert!(html.content.contains("1.0.228"), "{}", html.content);
    assert!(
        html.content.contains("https://docs.rs/serde"),
        "{}",
        html.content
    );
    assert!(!html.content.contains("<b>"), "{}", html.content);
    let json = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/api")}),
            context(),
        )
        .await
        .unwrap();
    assert!(
        json.content
            .contains(r#"{"crate":{"max_version":"1.0.228"}}"#),
        "{}",
        json.content
    );
    let heads = server.join().unwrap();
    assert!(
        heads[0].to_ascii_lowercase().contains("user-agent: scv/"),
        "{}",
        heads[0]
    );
    assert!(
        !heads[0].to_ascii_lowercase().contains("cookie"),
        "{}",
        heads[0]
    );
}

#[tokio::test]
async fn binary_content_is_refused_and_large_bodies_are_bounded_and_paged() {
    let long = "x".repeat(10_000);
    let (port, server) = serve(vec![
        http("200 OK", "image/png", "\u{89}PNG", ""),
        http("200 OK", "text/plain", &long, ""),
        http("200 OK", "text/plain", &long, ""),
    ]);
    let mut small = config();
    small.fetch_max_bytes = 4000;
    small.output_limit = 1500;
    let tool = fetch_tool(small, vec![port]);
    let binary = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/a.png")}),
            context(),
        )
        .await
        .unwrap();
    assert!(binary.is_error);
    assert!(binary.content.contains("image/png"), "{}", binary.content);
    let first = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/big")}),
            context(),
        )
        .await
        .unwrap();
    assert!(first.truncated);
    assert!(first.content.len() <= 1500, "{}", first.content.len());
    assert!(
        first.content.contains("download stopped at 4000 bytes"),
        "{}",
        first.content
    );
    let next: usize = first
        .content
        .rsplit("offset=")
        .next()
        .and_then(|rest| rest.split(' ').next())
        .unwrap()
        .parse()
        .unwrap();
    let second = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/big"),"offset":next}),
            context(),
        )
        .await
        .unwrap();
    assert!(
        second.content.contains(&format!("Characters: {next}-")),
        "{}",
        second.content
    );
    server.join().unwrap();
}

#[tokio::test]
async fn loopback_and_private_targets_are_refused_directly_by_name_and_by_redirect() {
    let (blocked_port, _unused) = serve(Vec::new());
    let (port, server) = serve(vec![http(
        "302 Found",
        "text/plain",
        "",
        &format!("Location: http://127.0.0.1:{blocked_port}/admin\r\n"),
    )]);
    let tool = fetch_tool(config(), vec![port]);
    let direct = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{blocked_port}/")}),
            context(),
        )
        .await
        .unwrap_err();
    assert!(direct.0.contains("non-public"), "{}", direct.0);
    let metadata = tool
        .execute(
            json!({"url":"http://169.254.169.254/latest/meta-data/"}),
            context(),
        )
        .await
        .unwrap_err();
    assert!(metadata.0.contains("non-public"), "{}", metadata.0);
    let named = tool
        .execute(
            json!({"url":format!("http://localhost:{port}/")}),
            context(),
        )
        .await
        .unwrap_err();
    assert!(named.0.contains("non-public"), "{}", named.0);
    let redirected = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/go")}),
            context(),
        )
        .await
        .unwrap_err();
    assert!(redirected.0.contains("non-public"), "{}", redirected.0);
    server.join().unwrap();
}

#[tokio::test]
async fn redirects_are_limited() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let looping = thread::spawn(move || {
        // The first request plus `max_redirects` (3) followed hops.
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                http(
                    "302 Found",
                    "text/plain",
                    "",
                    &format!("Location: http://127.0.0.1:{port}/again\r\n"),
                )
                .as_bytes(),
            );
        }
    });
    let tool = fetch_tool(config(), vec![port]);
    let error = tool
        .execute(
            json!({"url":format!("http://127.0.0.1:{port}/start")}),
            context(),
        )
        .await
        .unwrap_err();
    assert!(error.0.contains("redirects"), "{}", error.0);
    looping.join().unwrap();
}

#[test]
fn content_types_are_classified() {
    assert!(matches!(
        classify(Some("text/html; charset=utf-8"), b""),
        Ok(Body::Html)
    ));
    assert!(matches!(
        classify(Some("application/vnd.api+json"), b""),
        Ok(Body::Text)
    ));
    assert!(matches!(
        classify(Some("text/markdown"), b""),
        Ok(Body::Text)
    ));
    assert!(classify(Some("application/pdf"), b"").is_err());
    assert!(classify(Some("application/octet-stream"), b"").is_err());
    assert!(matches!(
        classify(None, b"<!DOCTYPE html><html>"),
        Ok(Body::Html)
    ));
    assert!(matches!(classify(None, b"plain words"), Ok(Body::Text)));
    assert!(classify(None, &[0xff, 0xfe, 0x00, 0x81, 0x90, 0xff, 0xfe, 0x00]).is_err());
}

#[test]
fn search_results_parse_for_each_backend() {
    let searxng = SearchBackend::Searxng {
        url: "http://s".into(),
    };
    let brave = SearchBackend::Brave {
        url: "http://b".into(),
        api_key: "brave-secret".into(),
    };
    assert_eq!(
            parse_results(
                &searxng,
                &json!({"results":[{"title":"Serde","url":"https://serde.rs","content":"A <b>framework</b>"},{"title":"no url"}]})
            )
            .unwrap(),
            vec![SearchResult {
                title: "Serde".into(),
                url: "https://serde.rs".into(),
                snippet: "A framework".into()
            }]
        );
    assert_eq!(
            parse_results(
                &brave,
                &json!({"web":{"results":[{"title":"Tokio &amp; async","url":"https://tokio.rs","description":"<strong>Tokio</strong> runtime"}]}})
            )
            .unwrap()[0],
            SearchResult {
                title: "Tokio & async".into(),
                url: "https://tokio.rs".into(),
                snippet: "Tokio runtime".into()
            }
        );
    assert!(parse_results(&brave, &json!({})).unwrap().is_empty());
    assert!(!format!("{brave:?}").contains("brave-secret"));
}

#[tokio::test]
async fn search_backends_are_queried_with_their_parameters() {
    let (port, server) = serve(vec![
        http(
            "200 OK",
            "application/json",
            r#"{"results":[{"title":"Serde","url":"https://serde.rs","content":"Serialization"}]}"#,
            "",
        ),
        http(
            "200 OK",
            "application/json",
            r#"{"web":{"results":[{"title":"Tokio","url":"https://tokio.rs","description":"Runtime"},{"title":"Two","url":"https://two.test","description":"x"}]}}"#,
            "",
        ),
        http("403 Forbidden", "text/plain", "forbidden", ""),
    ]);
    let mut searx = config();
    searx.search = Some(SearchBackend::Searxng {
        url: format!("http://127.0.0.1:{port}/"),
    });
    let tool = WebSearchTool {
        backend: searx.search.clone().unwrap(),
        config: Arc::new(searx),
    };
    assert_eq!(
        tool.risk(&json!({"query":"serde"})).unwrap(),
        ToolRisk::ReadOnly
    );
    assert!(tool.risk(&json!({"query":"  "})).is_err());
    let output = tool
        .execute(json!({"query":"serde json"}), context())
        .await
        .unwrap();
    assert!(
        output.content.contains("1. Serde\n   https://serde.rs"),
        "{}",
        output.content
    );

    let mut brave = config();
    brave.search = Some(SearchBackend::Brave {
        url: format!("http://127.0.0.1:{port}/res/v1/web/search"),
        api_key: "brave-test-key".into(),
    });
    let tool = WebSearchTool {
        backend: brave.search.clone().unwrap(),
        config: Arc::new(brave),
    };
    let output = tool
        .execute(json!({"query":"tokio","count":1}), context())
        .await
        .unwrap();
    assert!(output.content.contains("Tokio"), "{}", output.content);
    assert!(!output.content.contains("two.test"), "{}", output.content);
    let failure = tool
        .execute(json!({"query":"tokio"}), context())
        .await
        .unwrap();
    assert!(failure.is_error);
    assert!(failure.content.contains("API key"), "{}", failure.content);

    let heads = server.join().unwrap();
    assert!(
        heads[0].starts_with("GET /search?q=serde+json&format=json "),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with("GET /res/v1/web/search?q=tokio&count=1 "),
        "{}",
        heads[1]
    );
    assert!(
        heads[1]
            .to_ascii_lowercase()
            .contains("x-subscription-token: brave-test-key"),
        "{}",
        heads[1]
    );
}

#[test]
fn registration_offers_search_only_with_a_backend() {
    let mut registry = ToolRegistry::default();
    register(&mut registry, config()).unwrap();
    let names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    assert_eq!(names, vec!["web_fetch"]);
    let mut with_search = config();
    with_search.search = Some(SearchBackend::Searxng {
        url: "http://s".into(),
    });
    let mut registry = ToolRegistry::default();
    register(&mut registry, with_search).unwrap();
    let mut names: Vec<_> = registry.specs().into_iter().map(|spec| spec.name).collect();
    names.sort();
    assert_eq!(names, vec!["web_fetch", "web_search"]);
}
