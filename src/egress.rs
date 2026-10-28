//! Where the router may connect on a tenant's behalf.
//!
//! A cluster router serves environments the operator does not control, so a
//! tenant must not be able to make the router call an address only the operator
//! can reach: an in-cluster service, the router's own admin port, or a cloud
//! metadata endpoint. Two things steer the router's outbound requests:
//! a connection's `base_url`, and URLs inside a request (an image to fetch, a
//! Vertex `token_uri` inside a service-account key).
//!
//! Hostnames are checked where the connection is made, by [`PublicResolver`],
//! so a name that resolves to a private address is refused however it got
//! there, including a name that answers differently later (DNS rebinding).
//! IP literals never reach a resolver, so [`check_url`] refuses them up front.
//! [`guarded_client`] follows a redirect only to a URL [`check_url`] accepts,
//! since a public host could otherwise bounce an authenticated request to a
//! private one.
//!
//! Caller-supplied images use this policy in every mode. Provider connections
//! in the operator's own environment may still target private model services
//! such as an in-cluster vLLM; that trust never applies to image URLs.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, OnceLock},
    time::Duration,
};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether `address` is globally routable: not loopback, private, link-local,
/// carrier-grade NAT, documentation, multicast or otherwise reserved, and, for
/// the IPv6 forms that embed an IPv4 address, not an address that embeds one of
/// those.
pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || a == 0
        // 100.64.0.0/10 carrier-grade NAT
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 0)
        // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 documentation
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        // 198.18.0.0/15 benchmarking
        || (a == 198 && (18..20).contains(&b))
        // 240.0.0.0/4 reserved
        || a >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    // ::ffff:a.b.c.d and the deprecated ::a.b.c.d
    if let Some(mapped) = address.to_ipv4() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    let embedded = |high: u16, low: u16| {
        let [a, b] = high.to_be_bytes();
        let [c, d] = low.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        // fc00::/7 unique local
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10 link-local, fec0::/10 deprecated site-local
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        // 2001:db8::/32 documentation, 2001::/32 Teredo, 2001:2::/48 benchmarking,
        // 2001:10::/28 and 2001:20::/28 ORCHID
        || (segments[0] == 0x2001
            && (segments[1] == 0x0db8
                || segments[1] == 0
                || (segments[1] == 2 && segments[2] == 0)
                || (segments[1] & 0xfff0) == 0x0010
                || (segments[1] & 0xfff0) == 0x0020))
        // 3fff::/20 documentation, 5f00::/16 SRv6 segment identifiers
        || (segments[0] == 0x3fff && segments[1] < 0x1000)
        || segments[0] == 0x5f00
        // 100::/64 discard-only
        || (segments[0] == 0x0100 && segments[1..4] == [0, 0, 0])
    {
        return false;
    }
    // 64:ff9b:1::/48 is local-use NAT64: it carries a private IPv4 address.
    if segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2] == 1 {
        return false;
    }
    // 64:ff9b::/96 NAT64 and 2002::/16 6to4 carry an IPv4 address.
    if segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        return is_public_ipv4(embedded(segments[6], segments[7]));
    }
    if segments[0] == 0x2002 {
        return is_public_ipv4(embedded(segments[1], segments[2]));
    }
    true
}

/// Names that cannot be public. A friendly early refusal for the obvious cases;
/// the resolver is what actually decides.
fn is_local_name(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || !host.contains('.')
        || ["localhost", "local", "internal", "localdomain", "home.arpa"]
            .iter()
            .any(|suffix| host.ends_with(&format!(".{suffix}")))
}

/// Refuse a URL the router must not call from an environment it does not trust.
///
/// Only `http` and `https`, no embedded credentials, and no IP literal or
/// obviously local name that is not public. A hostname that merely resolves to
/// a private address passes here and is refused by [`PublicResolver`] when the
/// connection is made.
pub fn check_url(url: &str) -> Result<(), String> {
    let parsed =
        reqwest::Url::parse(url.trim()).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("URL scheme '{}' is not allowed", parsed.scheme()));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL must not contain credentials".to_string());
    }
    // `host_str` is the normalized host: the parser has already turned forms
    // such as `2130706433`, `0x7f.1` and `127.1` into dotted decimal.
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "URL has no host".to_string())?;
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(address) if is_public_ip(address) => Ok(()),
        Ok(address) => Err(format!("address {address} is not a public address")),
        Err(_) if is_local_name(host) => Err(format!("host '{host}' is not a public host")),
        Err(_) => Ok(()),
    }
}

/// Resolves a name and refuses it if any answer is not a public address.
/// Installed on [`guarded_client`], so it runs for the connection actually made.
pub struct PublicResolver;

/// Refuse a resolver answer that is empty or holds any non-public address.
///
/// One bad answer rejects the lot: a mixed answer is how an attacker gets a
/// private address picked.
fn vet_addresses(host: &str, addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    if addresses.is_empty() {
        return Err(format!("host '{host}' resolved to no addresses"));
    }
    if let Some(blocked) = addresses.iter().find(|a| !is_public_ip(a.ip())) {
        return Err(format!(
            "host '{host}' resolves to {}, which is not a public address",
            blocked.ip()
        ));
    }
    Ok(addresses)
}

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addresses: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let addresses = vet_addresses(&host, addresses)?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Redirects a guarded client will follow before giving up.
const MAX_REDIRECTS: usize = 5;

/// Follow a redirect only to a URL [`check_url`] accepts. A hostname in the
/// target is vetted again by [`PublicResolver`] when the connection is made, so
/// this is what stops a public host from bouncing a request to an IP literal.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        match check_url(attempt.url().as_str()) {
            Ok(()) => attempt.follow(),
            Err(error) => attempt.error(format!("redirect refused: {error}")),
        }
    })
}

fn guarded_builder(connect_timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .user_agent(crate::UPSTREAM_USER_AGENT)
        .dns_resolver(Arc::new(PublicResolver))
        .redirect(redirect_policy())
        // A proxy would resolve the destination itself, past `PublicResolver`.
        // Do not rely on reqwest's `system-proxy` feature staying disabled.
        .no_proxy()
}

/// A new guarded HTTP client, for a caller that needs its own connection pool
/// (see [`guarded_client`] for the shared one).
pub fn new_guarded_client(connect_timeout: Duration) -> reqwest::Client {
    guarded_builder(connect_timeout)
        .build()
        .expect("guarded outbound HTTP client")
}

/// An HTTP client for environments the operator does not control: public
/// destinations only. Redirects are followed only to public destinations. Built
/// once per process so every such environment shares one connection pool.
pub fn guarded_client(connect_timeout: Duration) -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| new_guarded_client(connect_timeout))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(address: &str) -> IpAddr {
        address.parse().unwrap()
    }

    #[test]
    fn reserved_and_internal_addresses_are_not_public() {
        for address in [
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "127.0.0.1",
            "127.1.2.3",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "2001:2::1",
            "2001:10::1",
            "2001:20::1",
            "3fff::1",
            "5f00::1",
            "100::1",
            // IPv6 forms that carry an IPv4 address
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::a00:5",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
        ] {
            assert!(!is_public_ip(ip(address)), "{address} must not be public");
        }
    }

    #[test]
    fn global_addresses_are_public() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "52.95.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.0.1",
            "172.32.0.1",
            "198.17.0.1",
            "198.20.0.1",
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "2001:4860:4860::8888",
            "3ffe::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(is_public_ip(ip(address)), "{address} must be public");
        }
    }

    #[test]
    fn urls_naming_internal_destinations_are_refused() {
        for url in [
            "http://127.0.0.1/v1",
            "http://localhost:8080/v1",
            "http://LOCALHOST./v1",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://10.0.0.5:8000/v1",
            // The WHATWG parser normalizes these to 127.0.0.1.
            "http://2130706433/",
            "http://0x7f.1/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://vllm.default.svc.cluster.local/v1",
            "http://vllm/v1",
            "http://printer.local/",
            "ftp://example.com/",
            "file:///etc/passwd",
            "https://user:secret@example.com/",
            "not a url",
        ] {
            assert!(check_url(url).is_err(), "{url} must be refused");
        }
    }

    #[test]
    fn public_urls_pass() {
        for url in [
            "https://api.openai.com/v1",
            "http://example.com:8080/v1/",
            "https://8.8.8.8/v1",
            "https://[2606:4700:4700::1111]/v1",
            "https://bedrock-runtime.us-west-2.amazonaws.com",
        ] {
            assert!(check_url(url).is_ok(), "{url} must pass");
        }
    }

    #[test]
    fn resolver_answers_are_refused_unless_every_address_is_public() {
        let socket = |address: &str| SocketAddr::new(ip(address), 0);
        let public = vec![socket("8.8.8.8"), socket("2606:4700:4700::1111")];
        assert_eq!(vet_addresses("example.com", public.clone()), Ok(public));

        for answer in [
            vec![],
            vec![socket("127.0.0.1")],
            vec![socket("::1")],
            vec![socket("169.254.169.254")],
            // A mixed answer is refused whichever order the private one is in.
            vec![socket("8.8.8.8"), socket("10.0.0.5")],
            vec![socket("10.0.0.5"), socket("8.8.8.8")],
        ] {
            assert!(
                vet_addresses("example.com", answer.clone()).is_err(),
                "{answer:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_guarded_client_refuses_names_that_resolve_to_private_addresses() {
        // `localhost` resolves to loopback through the system resolver, so it
        // exercises the connect-time check without any network access.
        let client = guarded_client(Duration::from_secs(2));
        let error = client
            .get("http://localhost:9/")
            .send()
            .await
            .expect_err("loopback must be refused");
        let message = format!("{error:?}");
        assert!(message.contains("not a public address"), "{message}");
    }

    #[tokio::test]
    async fn the_guarded_client_refuses_redirects_to_non_public_addresses() {
        use axum::{response::Redirect, routing::get, Router};
        let app = Router::new().route(
            "/",
            get(|| async { Redirect::temporary("http://169.254.169.254/") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // An IP literal never reaches the resolver, so this reaches the loopback
        // test server while keeping the guarded client's redirect policy.
        let client = guarded_builder(Duration::from_secs(2)).build().unwrap();
        let error = client
            .get(format!("http://{address}/"))
            .send()
            .await
            .expect_err("a redirect to the metadata address must be refused");
        let message = format!("{error:?}");
        assert!(message.contains("redirect refused"), "{message}");
    }
}
