use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("sk1ll-server {}", sk1ll_core::VERSION);
            return Ok(());
        }
        Some("--healthcheck") => std::process::exit(healthcheck()),
        // Run by the bare repo's pre-receive hook, in the repo.
        Some("--check-push") => {
            let Some(commit) = std::env::args().nth(2) else {
                eprintln!("sk1ll-server: --check-push needs the pushed commit");
                std::process::exit(2);
            };
            if let Err(e) = sk1ll_server::check_push(std::path::Path::new("."), &commit).await {
                eprintln!("sk1ll-server: {e:#}");
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    sk1ll_server::run(sk1ll_server::Config::from_env()?).await
}

/// Asks the server running here (at `SK1LL_SERVER_BIND`) for `/healthz`: exit
/// status 0 if it answers 200, so container healthchecks need no curl.
fn healthcheck() -> i32 {
    let check = || -> anyhow::Result<bool> {
        let bind = sk1ll_server::Config::from_env()?.bind;
        let ip = match bind.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        };
        let timeout = Duration::from_secs(5);
        let mut stream = TcpStream::connect_timeout(&SocketAddr::new(ip, bind.port()), timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response.starts_with("HTTP/1.1 200 "))
    };
    match check() {
        Ok(true) => 0,
        Ok(false) => {
            eprintln!("sk1ll-server is unhealthy");
            1
        }
        Err(e) => {
            eprintln!("sk1ll-server is unreachable: {e:#}");
            1
        }
    }
}
