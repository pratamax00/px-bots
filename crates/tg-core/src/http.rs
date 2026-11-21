use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// DNS resolver that prioritizes IPv4 addresses to avoid IPv6 blackholes.
pub struct Ipv4FirstResolver;

impl ureq::Resolver for Ipv4FirstResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let mut addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        addrs.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
        Ok(addrs)
    }
}

/// Builds a blocking `ureq::Agent` for continuous long-polling connections.
/// Sets explicit connect, write, read, and total wall-clock timeouts.
pub fn build_agent(connect_secs: u64, read_secs: u64) -> ureq::Agent {
    let write_secs = 15;
    let total_secs = connect_secs
        .saturating_add(read_secs)
        .saturating_add(write_secs);
    ureq::AgentBuilder::new()
        .resolver(Ipv4FirstResolver)
        .timeout_connect(Duration::from_secs(connect_secs))
        .timeout_write(Duration::from_secs(write_secs))
        .timeout_read(Duration::from_secs(read_secs))
        .timeout(Duration::from_secs(total_secs))
        .max_idle_connections(1)
        .max_idle_connections_per_host(1)
        .build()
}

/// Builds a short-lived `ureq::Agent` for bursty external API requests (Telegram send/edit,
/// Cloudflare Workers, monitoring probes, router APIs).
/// Disables idle connection pooling (`max_idle_connections(0)`) so requests never stall on
/// half-open sockets silently dropped by home-router NAT or edge proxies during idle periods.
pub fn build_api_agent(timeout_secs: u64) -> ureq::Agent {
    let total = timeout_secs.max(5);
    let connect = total.min(8);
    let write = total.min(10);
    ureq::AgentBuilder::new()
        .resolver(Ipv4FirstResolver)
        .timeout_connect(Duration::from_secs(connect))
        .timeout_write(Duration::from_secs(write))
        .timeout_read(Duration::from_secs(total))
        .timeout(Duration::from_secs(total))
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .build()
}
