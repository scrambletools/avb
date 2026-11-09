//! The helper for raw Ethernet, the one file granted CAP_NET_RAW
//! (`setcap cap_net_raw+ep avb-netraw`): it opens packet sockets for
//! avb-endpointd and the apps' controllers and passes them over, so they
//! hold no capability themselves and the desktop's portals still answer
//! the apps.

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(avb_net::broker::serve());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("avb-netraw is for Linux, which grants raw sockets by capability");
    std::process::exit(2);
}
