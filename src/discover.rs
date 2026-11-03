//! Finds a running browser's DevTools socket from the `DevToolsActivePort` file that Chromium writes
//! into its user-data directory while remote debugging is allowed.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default user-data directories, most common first, relative to the home directory.
#[cfg(target_os = "macos")]
const PROFILES: &[(&str, &str)] = &[
    ("Chrome", "Library/Application Support/Google/Chrome"),
    (
        "Brave",
        "Library/Application Support/BraveSoftware/Brave-Browser",
    ),
    ("Edge", "Library/Application Support/Microsoft Edge"),
    (
        "Chrome Beta",
        "Library/Application Support/Google/Chrome Beta",
    ),
    (
        "Chrome Canary",
        "Library/Application Support/Google/Chrome Canary",
    ),
    ("Chromium", "Library/Application Support/Chromium"),
];
#[cfg(windows)]
const PROFILES: &[(&str, &str)] = &[
    ("Chrome", r"Google\Chrome\User Data"),
    ("Brave", r"BraveSoftware\Brave-Browser\User Data"),
    ("Edge", r"Microsoft\Edge\User Data"),
    ("Chrome Beta", r"Google\Chrome Beta\User Data"),
    ("Chrome Canary", r"Google\Chrome SxS\User Data"),
    ("Chromium", r"Chromium\User Data"),
];
#[cfg(not(any(target_os = "macos", windows)))]
const PROFILES: &[(&str, &str)] = &[
    ("Chrome", ".config/google-chrome"),
    ("Brave", ".config/BraveSoftware/Brave-Browser"),
    ("Edge", ".config/microsoft-edge"),
    ("Chrome Beta", ".config/google-chrome-beta"),
    ("Chromium", ".config/chromium"),
];

/// A user-data directory and what its `DevToolsActivePort` says, if anything.
pub struct Candidate {
    pub name: String,
    pub dir: PathBuf,
    /// `ws://` endpoint read from the file, when present.
    pub endpoint: Option<String>,
    /// Whether something is listening on that port right now (the file can outlive the browser).
    pub live: bool,
}

/// Reads `DevToolsActivePort`: line 1 is the port, line 2 the browser socket path.
pub fn endpoint_in(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("DevToolsActivePort")).ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next().map(str::trim).filter(|p| p.starts_with('/'))?;
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

fn is_live(endpoint: &str) -> bool {
    let Ok((addr, _)) = crate::ws::parse_url(endpoint) else {
        return false;
    };
    addr.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(300)).is_ok())
}

pub fn inspect(name: &str, dir: PathBuf) -> Candidate {
    let endpoint = endpoint_in(&dir);
    let live = endpoint.as_deref().is_some_and(is_live);
    Candidate {
        name: name.to_string(),
        dir,
        endpoint,
        live,
    }
}

/// Every default browser profile on this machine, in priority order.
pub fn candidates() -> Vec<Candidate> {
    // Windows keeps profiles under %LOCALAPPDATA%; elsewhere the paths are relative to $HOME.
    let base = if cfg!(windows) {
        "LOCALAPPDATA"
    } else {
        "HOME"
    };
    let Some(home) = std::env::var_os(base).map(PathBuf::from) else {
        return Vec::new();
    };
    PROFILES
        .iter()
        .map(|(name, rel)| inspect(name, home.join(rel)))
        .collect()
}

/// The endpoint to use: an explicit one, else the given profile, else the first live default.
pub fn resolve(ws_endpoint: Option<&str>, user_data_dir: Option<&Path>) -> Result<String, String> {
    if let Some(ep) = ws_endpoint {
        return Ok(ep.to_string());
    }
    if let Some(dir) = user_data_dir {
        return endpoint_in(dir).ok_or_else(|| {
            format!(
                "no DevToolsActivePort in {}: allow remote debugging in that browser \
                 (chrome://inspect/#remote-debugging) and keep it running",
                dir.display()
            )
        });
    }
    candidates()
        .into_iter()
        .find(|c| c.live)
        .and_then(|c| c.endpoint)
        .ok_or_else(|| {
            "no running browser with remote debugging found. Open chrome://inspect/#remote-debugging \
             (brave://inspect for Brave), allow remote debugging, or pass --user-data-dir / --ws-endpoint"
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_active_port_file() {
        let dir = std::env::temp_dir().join(format!("wmb-port-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("DevToolsActivePort"),
            "9222\n/devtools/browser/abc\n",
        )
        .unwrap();
        assert_eq!(
            endpoint_in(&dir).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc")
        );
        std::fs::write(dir.join("DevToolsActivePort"), "not-a-port\n/x\n").unwrap();
        assert_eq!(endpoint_in(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
