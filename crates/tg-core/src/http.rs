use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// Resolver DNS yang selalu mengutamakan IPv4 untuk menghindari IPv6 blackhole.
pub struct Ipv4FirstResolver;

impl ureq::Resolver for Ipv4FirstResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let mut addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        addrs.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
        Ok(addrs)
    }
}

/// Membuat `ureq::Agent` dengan IPv4-first resolver + timeout terukur.
pub fn build_agent(connect_secs: u64, read_secs: u64) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .resolver(Ipv4FirstResolver)
        .timeout_connect(Duration::from_secs(connect_secs))
        .timeout_read(Duration::from_secs(read_secs))
        .build()
}
