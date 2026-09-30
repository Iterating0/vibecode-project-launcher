use url::{Host, Url};

/// Open a validated local development URL with the user's default browser.
/// The call is made from the ProjectManager worker so IPC threads stay free.
pub fn open_url(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|error| format!("invalid browser URL: {error}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("browser URL must use http or https".to_owned());
    }
    let loopback = match parsed.host() {
        Some(Host::Domain(host)) => host.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if !loopback {
        return Err("browser URL must point to localhost".to_owned());
    }
    open::that(url).map_err(|error| format!("failed to open browser: {error}"))
}

pub fn render_url(template: &str, port: u16) -> Result<String, String> {
    if !template.contains("{port}") {
        return Err("url template must contain {port}".to_owned());
    }
    let rendered = template.replace("{port}", &port.to_string());
    Url::parse(&rendered)
        .map(|_| rendered)
        .map_err(|error| format!("invalid rendered URL: {error}"))
}
