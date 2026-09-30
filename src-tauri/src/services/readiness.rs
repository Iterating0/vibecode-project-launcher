use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};
use url::{Host, Url};

/// Probe only during startup; cancellation and process ownership are checked
/// again before a successful response can open a browser.
pub fn wait_for_http_ready<F, C>(
    url: &str,
    timeout: Duration,
    mut owns_listener: F,
    mut cancelled: C,
) -> Result<(), String>
where
    F: FnMut() -> bool,
    C: FnMut() -> bool,
{
    let parsed = Url::parse(url).map_err(|error| format!("Invalid readiness URL: {error}"))?;
    let addresses = loopback_addresses(&parsed)?;
    let host_header = &parsed[url::Position::BeforeHost..url::Position::AfterPort];
    let resource = &parsed[url::Position::BeforePath..url::Position::AfterQuery];
    let request =
        format!("GET {resource} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cancelled() {
            return Err("Startup cancelled".into());
        }
        if owns_listener() {
            for address in &addresses {
                if cancelled() {
                    return Err("Startup cancelled".into());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                if probe_http(
                    *address,
                    &request,
                    remaining.min(Duration::from_millis(250)),
                ) && !cancelled()
                    && owns_listener()
                {
                    return Ok(());
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("Timed out waiting for an HTTP response from the project's allocated port".into())
}

fn loopback_addresses(url: &Url) -> Result<Vec<SocketAddr>, String> {
    if url.scheme() != "http" {
        return Err("This launcher supports local HTTP URLs".into());
    }
    let port = url.port_or_known_default().ok_or("URL has no port")?;
    match url.host().ok_or("URL has no host")? {
        Host::Ipv4(ip) if ip.is_loopback() => Ok(vec![SocketAddr::new(ip.into(), port)]),
        Host::Ipv6(ip) if ip.is_loopback() => Ok(vec![SocketAddr::new(ip.into(), port)]),
        Host::Domain(host) if host.trim_end_matches('.').eq_ignore_ascii_case("localhost") => {
            let addresses: Vec<_> = ("localhost", port)
                .to_socket_addrs()
                .map_err(|error| format!("Cannot resolve localhost: {error}"))?
                .filter(|address| address.ip().is_loopback())
                .collect();
            if addresses.is_empty() {
                Err("localhost has no loopback address".into())
            } else {
                Ok(addresses)
            }
        }
        _ => Err("URL must point to a loopback address".into()),
    }
}

fn probe_http(address: SocketAddr, request: &str, timeout: Duration) -> bool {
    let Ok(mut socket) = TcpStream::connect_timeout(&address, timeout) else {
        return false;
    };
    if socket.set_read_timeout(Some(timeout)).is_err()
        || socket.set_write_timeout(Some(timeout)).is_err()
    {
        return false;
    }
    if socket.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut line = Vec::with_capacity(64);
    let read_deadline = Instant::now() + timeout;
    let mut complete = false;
    while line.len() < 512 && Instant::now() < read_deadline {
        let mut byte = [0u8];
        match socket.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => {
                complete = true;
                break;
            }
            Ok(1) => line.push(byte[0]),
            _ => return false,
        }
    }
    if !complete {
        return false;
    }
    let Ok(line) = std::str::from_utf8(&line) else {
        return false;
    };
    let mut fields = line.split_whitespace();
    let protocol = fields.next().unwrap_or_default();
    let status = fields.next().and_then(|code| code.parse::<u16>().ok());
    matches!(protocol, "HTTP/1.0" | "HTTP/1.1")
        && status.is_some_and(|code| (200..500).contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn real_http_response_with_query_is_ready() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let read = socket.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read])
                .starts_with("GET /health?ready=1 HTTP/1.1\r\n"));
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                .unwrap();
        });
        wait_for_http_ready(
            &format!("http://{address}/health?ready=1"),
            Duration::from_secs(2),
            || true,
            || false,
        )
        .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn cancellation_does_not_wait_for_timeout() {
        let started = Instant::now();
        assert!(wait_for_http_ready(
            "http://localhost:9",
            Duration::from_secs(30),
            || false,
            || true
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn ipv6_and_loopback_range_parse() {
        assert_eq!(
            loopback_addresses(&Url::parse("http://[::1]:5173").unwrap()).unwrap(),
            vec!["[::1]:5173".parse::<SocketAddr>().unwrap()]
        );
        assert!(loopback_addresses(&Url::parse("http://127.0.0.2:5173").unwrap()).is_ok());
        assert!(loopback_addresses(&Url::parse("http://example.com:5173").unwrap()).is_err());
    }
}
