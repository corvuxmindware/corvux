//! DNS for the sync engine's iroh endpoint: ask the system resolver first, and
//! only if that fails ask public resolvers.
//!
//! Some ISPs refuse queries for iroh's discovery host: Reliance Jio's resolver
//! answers REFUSED for `dns.iroh.link` (seen 2026-11-05), so with iroh's default
//! system-only resolver the N0 pkarr publish and DNS address lookup failed on a
//! retry loop. Simply adding public nameservers to hickory's pool stopped
//! helping in hickory-resolver 0.26.3 (a refusal no longer moves on to the next
//! server), so the fallback is an explicit second step instead. That also keeps
//! the public resolvers out of the picture whenever the system one answers.
//!
//! Only iroh's own relay/discovery hostnames go through this resolver, never
//! anything about note content.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::time::Duration;

use iroh::dns::{BoxIter, DnsError, DnsProtocol, DnsResolver, Resolver, TxtRecordData};

/// Public resolvers asked only after the system resolver fails.
pub const SYNC_DNS_FALLBACKS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53),
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53),
];

/// Budget for the system resolver. It normally answers in milliseconds and a
/// refusal is immediate; together with [`FALLBACK_TIMEOUT`] this stays inside
/// iroh's own 3 s DNS timeout, so the fallback gets its turn even when the
/// system resolver is unreachable rather than refusing.
const SYSTEM_TIMEOUT: Duration = Duration::from_secs(1);
/// Budget for the public fallback resolvers.
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(2);

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// The resolver handed to the iroh endpoint in `build_endpoint`.
pub fn sync_dns_resolver() -> DnsResolver {
    DnsResolver::custom(SystemThenPublic::new())
}

/// Asks `system` first and `public` only when `system` returns an error.
#[derive(Debug, Clone)]
pub struct SystemThenPublic {
    system: DnsResolver,
    public: DnsResolver,
}

impl SystemThenPublic {
    /// The system DNS configuration, then [`SYNC_DNS_FALLBACKS`].
    pub fn new() -> Self {
        Self::with_parts(
            DnsResolver::builder().with_system_defaults().build(),
            DnsResolver::builder()
                .with_nameservers(SYNC_DNS_FALLBACKS.map(|addr| (addr, DnsProtocol::Udp)))
                .build(),
        )
    }

    /// Builds from explicit resolvers (the tests swap in fakes).
    pub fn with_parts(system: DnsResolver, public: DnsResolver) -> Self {
        Self { system, public }
    }
}

impl Default for SystemThenPublic {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver for SystemThenPublic {
    fn lookup_ipv4(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
        let this = self.clone();
        Box::pin(async move {
            let addrs: Vec<IpAddr> = match this.system.lookup_ipv4(&host, SYSTEM_TIMEOUT).await {
                Ok(found) => found.collect(),
                Err(_) => this
                    .public
                    .lookup_ipv4(&host, FALLBACK_TIMEOUT)
                    .await?
                    .collect(),
            };
            let v4 = addrs.into_iter().filter_map(|ip| match ip {
                IpAddr::V4(v4) => Some(v4),
                IpAddr::V6(_) => None,
            });
            Ok(Box::new(v4) as BoxIter<Ipv4Addr>)
        })
    }

    fn lookup_ipv6(&self, host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
        let this = self.clone();
        Box::pin(async move {
            let addrs: Vec<IpAddr> = match this.system.lookup_ipv6(&host, SYSTEM_TIMEOUT).await {
                Ok(found) => found.collect(),
                Err(_) => this
                    .public
                    .lookup_ipv6(&host, FALLBACK_TIMEOUT)
                    .await?
                    .collect(),
            };
            let v6 = addrs.into_iter().filter_map(|ip| match ip {
                IpAddr::V6(v6) => Some(v6),
                IpAddr::V4(_) => None,
            });
            Ok(Box::new(v6) as BoxIter<Ipv6Addr>)
        })
    }

    fn lookup_txt(&self, host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
        let this = self.clone();
        Box::pin(async move {
            let records: Vec<TxtRecordData> =
                match this.system.lookup_txt(&host, SYSTEM_TIMEOUT).await {
                    Ok(found) => found.collect(),
                    Err(_) => this
                        .public
                        .lookup_txt(&host, FALLBACK_TIMEOUT)
                        .await?
                        .collect(),
                };
            Ok(Box::new(records.into_iter()) as BoxIter<TxtRecordData>)
        })
    }

    fn clear_cache(&self) {
        self.system.clear_cache();
        self.public.clear_cache();
    }

    /// Rebuilt from scratch so the system half re-reads the OS DNS settings
    /// after a network change. Building does no IO.
    fn reset(&self) -> Box<dyn Resolver> {
        Box::new(Self::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Answers every IPv4 lookup with one fixed address and counts the calls.
    #[derive(Debug, Clone)]
    struct Fixed {
        addr: Ipv4Addr,
        calls: Arc<AtomicUsize>,
    }

    impl Fixed {
        fn new(addr: Ipv4Addr) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    addr,
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    impl Resolver for Fixed {
        fn lookup_ipv4(&self, _host: String) -> BoxFuture<Result<BoxIter<Ipv4Addr>, DnsError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let addr = self.addr;
            Box::pin(async move { Ok(Box::new(std::iter::once(addr)) as BoxIter<Ipv4Addr>) })
        }
        fn lookup_ipv6(&self, _host: String) -> BoxFuture<Result<BoxIter<Ipv6Addr>, DnsError>> {
            Box::pin(async { Ok(Box::new(std::iter::empty()) as BoxIter<Ipv6Addr>) })
        }
        fn lookup_txt(&self, _host: String) -> BoxFuture<Result<BoxIter<TxtRecordData>, DnsError>> {
            Box::pin(async { Ok(Box::new(std::iter::empty()) as BoxIter<TxtRecordData>) })
        }
        fn clear_cache(&self) {}
        fn reset(&self) -> Box<dyn Resolver> {
            Box::new(self.clone())
        }
    }

    /// A resolver pointed at TEST-NET-1, which never answers.
    fn dead() -> DnsResolver {
        DnsResolver::builder()
            .with_nameserver(
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 53),
                DnsProtocol::Udp,
            )
            .build()
    }

    async fn lookup(resolver: SystemThenPublic) -> Result<Vec<IpAddr>, DnsError> {
        DnsResolver::custom(resolver)
            .lookup_ipv4("example.test", Duration::from_secs(5))
            .await
            .map(|found| found.collect())
    }

    #[tokio::test]
    async fn system_answer_wins_and_public_is_not_asked() {
        let (system, system_calls) = Fixed::new(Ipv4Addr::new(10, 0, 0, 1));
        let (public, public_calls) = Fixed::new(Ipv4Addr::new(10, 0, 0, 2));
        let got = lookup(SystemThenPublic::with_parts(
            DnsResolver::custom(system),
            DnsResolver::custom(public),
        ))
        .await
        .expect("system answers");
        assert_eq!(got, [IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))]);
        assert_eq!(system_calls.load(Ordering::SeqCst), 1);
        assert_eq!(public_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn falls_back_to_public_when_system_fails() {
        let (public, public_calls) = Fixed::new(Ipv4Addr::new(10, 0, 0, 2));
        let got = lookup(SystemThenPublic::with_parts(
            dead(),
            DnsResolver::custom(public),
        ))
        .await
        .expect("public answers after the system resolver fails");
        assert_eq!(got, [IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))]);
        assert_eq!(public_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fails_when_both_fail() {
        assert!(lookup(SystemThenPublic::with_parts(dead(), dead()))
            .await
            .is_err());
    }

    #[test]
    fn timeouts_fit_inside_iroh_dns_budget() {
        assert!(SYSTEM_TIMEOUT + FALLBACK_TIMEOUT <= Duration::from_secs(3));
    }
}
