//! End to end against a real Chromium: a local page registers WebMCP tools, the bridge serves them.
//! Runs only when WEBMCP_CHROME points at a Chrome/Chromium binary with WebMCP (Chrome 150+).

mod common;

use common::Mcp;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PAGE: &str = r#"<!doctype html><title>tabwire test</title><script>
const mc = document.modelContext || navigator.modelContext;
mc.registerTool({ name: 'echo', description: 'Echo text back', annotations: { readOnlyHint: true },
  inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
  execute: async ({ text }) => JSON.stringify({ echo: text }) });
mc.registerTool({ name: 'fail', description: 'Always fails', inputSchema: { type: 'object', properties: {} },
  execute: async () => { throw new Error('nope') } });
mc.registerTool({ name: 'reload', description: 'Reload the page', inputSchema: { type: 'object', properties: {} },
  execute: async () => { setTimeout(() => location.reload(), 50); return 'reloading' } });
</script>"#;

struct Cleanup(Vec<Child>, PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn serve_page() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}", PAGE.len());
        }
    });
    port
}

#[test]
fn real_browser_end_to_end() {
    let Ok(chrome) = std::env::var("WEBMCP_CHROME") else {
        eprintln!("skipped: set WEBMCP_CHROME to a Chrome binary to run the real-browser test");
        return;
    };
    let port = serve_page();
    let dir = std::env::temp_dir().join(format!("wmb-chrome-{}", std::process::id()));
    let browser = Command::new(chrome)
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            &format!("--user-data-dir={}", dir.display()),
            "--enable-features=WebMCPTesting,DevToolsWebMCPSupport",
            "--no-first-run",
            "--no-default-browser-check",
            &format!("http://127.0.0.1:{port}/"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start chrome");
    let mut cleanup = Cleanup(vec![browser], dir.clone());
    let deadline = Instant::now() + Duration::from_secs(15);
    while !dir.join("DevToolsActivePort").exists() {
        assert!(
            Instant::now() < deadline,
            "chrome never wrote DevToolsActivePort"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let (mut mcp, child) = Mcp::start(&[
        "--user-data-dir",
        dir.to_str().unwrap(),
        "--url",
        "127.0.0.1",
    ]);
    cleanup.0.push(child);
    mcp.initialize();

    // The page registers its tools as it loads; wait for them.
    let deadline = Instant::now() + Duration::from_secs(15);
    let names = loop {
        let names = mcp.tool_names();
        if names.len() == 3 || Instant::now() > deadline {
            break names;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(names, ["echo", "fail", "reload"], "tools served by name");
    let list = mcp.request("tools/list", json!({}));
    let echo = &list["result"]["tools"][0];
    assert_eq!(
        echo["inputSchema"]["required"][0], "text",
        "schema passed through: {echo}"
    );

    let r = mcp.request(
        "tools/call",
        json!({ "name": "echo", "arguments": { "text": "hi" } }),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(
        r["result"]["content"][0]["text"], "{\"echo\":\"hi\"}",
        "{r}"
    );

    let r = mcp.request("tools/call", json!({ "name": "fail", "arguments": {} }));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(
        r["result"]["content"][0]["text"], "Error: nope",
        "the page's own error message: {r}"
    );

    // Latency through the whole chain: MCP -> bridge -> browser -> page -> back.
    let n = 200;
    let t0 = Instant::now();
    for i in 0..n {
        let r = mcp.request(
            "tools/call",
            json!({ "name": "echo", "arguments": { "text": i.to_string() } }),
        );
        assert_eq!(r["result"]["isError"], false);
    }
    let per_call = t0.elapsed() / n;
    eprintln!("real browser round trip: {per_call:?} per tool call ({n} calls)");

    // A reload drops and re-registers the tools: the client must hear about it.
    mcp.notifications.clear();
    let r = mcp.request("tools/call", json!({ "name": "reload", "arguments": {} }));
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert!(
        mcp.wait_notification("notifications/tools/list_changed", Duration::from_secs(10)),
        "list_changed after reload"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while mcp.tool_names().len() != 3 {
        assert!(
            Instant::now() < deadline,
            "tools came back after the reload"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
