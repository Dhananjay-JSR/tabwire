//! webmcp-bridge: serves the WebMCP tools of the pages open in your browser to any MCP client.

mod bridge;
mod cdp;
mod discover;
mod ws;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const HELP: &str =
    "webmcp-bridge: give MCP clients the WebMCP tools of the pages open in your browser.

USAGE
  webmcp-bridge [OPTIONS]          serve MCP over stdio (what your agent runs)
  webmcp-bridge doctor [OPTIONS]   check the browser setup and list the tools it would serve

OPTIONS
  --url <text>             only pages whose URL contains <text> (repeatable), e.g. --url tldraw.com
  --user-data-dir <path>   the browser profile folder to attach to (default: first running
                           Chrome, Brave, Edge or Chromium with remote debugging allowed)
  --ws-endpoint <url>      attach to this ws://127.0.0.1:<port>/devtools/browser/<id> instead
  --timeout <seconds>      longest a tool call may run (default 120)
  -h, --help               show this help
  -V, --version            show the version

The browser needs WebMCP enabled (chrome://flags/#enable-webmcp-testing set to Enabled) and remote
debugging allowed (chrome://inspect/#remote-debugging). Brave: the same pages under brave://.";

struct Args {
    doctor: bool,
    cfg: bridge::Config,
}

fn parse_args(raw: Vec<String>) -> Result<Option<Args>, String> {
    let mut cfg = bridge::Config {
        ws_endpoint: None,
        user_data_dir: None,
        urls: Vec::new(),
        call_timeout: Duration::from_secs(120),
    };
    let mut doctor = false;
    let mut it = raw.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("webmcp-bridge {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "doctor" => doctor = true,
            "serve" => {}
            "--url" => cfg.urls.push(value("--url")?),
            "--user-data-dir" => cfg.user_data_dir = Some(PathBuf::from(value("--user-data-dir")?)),
            "--ws-endpoint" => cfg.ws_endpoint = Some(value("--ws-endpoint")?),
            "--timeout" => {
                let secs: u64 = value("--timeout")?
                    .parse()
                    .map_err(|_| "--timeout must be a whole number of seconds")?;
                if secs == 0 {
                    return Err("--timeout must be at least 1".into());
                }
                cfg.call_timeout = Duration::from_secs(secs);
            }
            other => return Err(format!("unknown argument {other} (see --help)")),
        }
    }
    Ok(Some(Args { doctor, cfg }))
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1).collect()) {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("webmcp-bridge: {e}");
            return ExitCode::from(2);
        }
    };
    if args.doctor {
        return doctor(&args.cfg);
    }
    let bridge = bridge::Bridge::new(args.cfg);
    let manager = bridge.clone();
    std::thread::Builder::new()
        .name("manager".into())
        .spawn(move || manager.run_manager())
        .expect("start the manager thread");
    bridge.serve();
    ExitCode::SUCCESS
}

/// Prints what the bridge would find, step by step, with the fix for each missing piece.
fn doctor(cfg: &bridge::Config) -> ExitCode {
    println!("webmcp-bridge {} doctor\n", env!("CARGO_PKG_VERSION"));
    println!("Browser profiles:");
    let mut list = discover::candidates();
    if let Some(dir) = &cfg.user_data_dir {
        list.insert(0, discover::inspect("--user-data-dir", dir.clone()));
    }
    for c in &list {
        let state = match (&c.endpoint, c.live) {
            (_, true) => "remote debugging ON",
            (Some(_), false) => "stale DevToolsActivePort (browser not running?)",
            (None, _) if c.dir.exists() => "remote debugging off",
            (None, _) => "not installed",
        };
        println!("  {:<14} {}  ({})", c.name, state, c.dir.display());
    }
    let endpoint = match discover::resolve(cfg.ws_endpoint.as_deref(), cfg.user_data_dir.as_deref())
    {
        Ok(e) => e,
        Err(e) => {
            println!("\n✗ {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("\nConnecting to {endpoint} … (approve the prompt in the browser if it asks)");
    let (cdp, rx) = match cdp::connect(&endpoint) {
        Ok(x) => x,
        Err(e) => {
            println!("✗ {e}");
            return ExitCode::FAILURE;
        }
    };
    let targets = match cdp.call("Target.getTargets", json!({}), None) {
        Ok(r) => r["targetInfos"].as_array().cloned().unwrap_or_default(),
        Err(e) => {
            println!("✗ {e}");
            return ExitCode::FAILURE;
        }
    };
    let pages: Vec<&Value> = targets.iter().filter(|t| t["type"] == "page").collect();
    let wanted: Vec<&&Value> = pages
        .iter()
        .filter(|t| cfg.wants(t["url"].as_str().unwrap_or_default()))
        .collect();
    println!(
        "✓ connected: {} tabs, {} match the URL filter",
        pages.len(),
        wanted.len()
    );
    let mut total = 0;
    let mut webmcp_off = false;
    for page in wanted {
        let url = page["url"].as_str().unwrap_or_default();
        let session = match cdp.call(
            "Target.attachToTarget",
            json!({ "targetId": page["targetId"], "flatten": true }),
            None,
        ) {
            Ok(r) => r["sessionId"].as_str().unwrap_or_default().to_string(),
            Err(e) => {
                println!("  {url}: ✗ {e}");
                continue;
            }
        };
        if let Err(e) = cdp.call("WebMCP.enable", json!({}), Some(&session)) {
            println!("  {url}: ✗ {e}");
            webmcp_off = true;
            continue;
        }
        let mut names = Vec::new();
        let until = Instant::now() + Duration::from_millis(400);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(cdp::Incoming::Event(ev))
                    if ev["method"] == "WebMCP.toolsAdded"
                        && ev["sessionId"] == session.as_str() =>
                {
                    names.extend(
                        ev["params"]["tools"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|t| t["name"].as_str().map(String::from)),
                    );
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = cdp.call(
            "Target.detachFromTarget",
            json!({ "sessionId": session }),
            None,
        );
        total += names.len();
        println!(
            "  {url}: {} tool(s){}{}",
            names.len(),
            if names.is_empty() { "" } else { ": " },
            names.join(", ")
        );
    }
    if webmcp_off {
        println!("\n✗ WebMCP is not available in this browser. Set chrome://flags/#enable-webmcp-testing to Enabled\n  (Brave: brave://flags, choose \"Enabled\", not \"Default\") and relaunch.");
        return ExitCode::FAILURE;
    }
    println!(
        "\n{} {total} tool(s) would be served.",
        if total > 0 { "✓" } else { "!" }
    );
    if total == 0 {
        println!("  Open a page that registers WebMCP tools (e.g. tldraw.com with GhostDraw) and run doctor again.");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> Result<Option<Args>, String> {
        parse_args(a.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn flags_parse_and_bad_input_is_explained() {
        let a = parse(&[
            "doctor",
            "--url",
            "tldraw.com",
            "--url",
            "x.dev",
            "--timeout",
            "30",
        ])
        .unwrap()
        .unwrap();
        assert!(a.doctor);
        assert_eq!(a.cfg.urls, ["tldraw.com", "x.dev"]);
        assert_eq!(a.cfg.call_timeout, Duration::from_secs(30));
        assert!(parse(&["--url"]).err().unwrap().contains("needs a value"));
        assert!(parse(&["--timeout", "0"]).is_err());
        assert!(parse(&["--nope"])
            .err()
            .unwrap()
            .contains("unknown argument"));
    }
}
