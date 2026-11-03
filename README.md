# webmcp-bridge

Give Claude Code, Codex, Zed or any MCP client the **WebMCP tools of the pages open in your browser**, and nothing else.

Web pages can register tools for AI agents with [WebMCP](https://github.com/webmachinelearning/webmcp) (`document.modelContext.registerTool`). `webmcp-bridge` is a small Rust binary that finds those tools in your running Chrome or Brave and serves them to your agent as ordinary MCP tools, under their real names.

```
Agent (MCP over stdio) ──▶ webmcp-bridge ──▶ your browser (DevTools protocol, localhost) ──▶ the page's tools
```

## Why not chrome-devtools-mcp?

[chrome-devtools-mcp](https://github.com/ChromeDevTools/chrome-devtools-mcp) can reach WebMCP tools too, with `--categoryExperimentalWebmcp`. It is a general browser-automation server, so:

| | chrome-devtools-mcp | webmcp-bridge |
|---|---|---|
| Tools your agent sees | 8-34 browser tools (click, navigate, evaluate, screenshot, ...) plus a two-step proxy | Only the page's tools, by name (e.g. `tldraw_pen`) |
| Calling a page tool | `list_webmcp_tools`, then `execute_webmcp_tool` with the input as a JSON string | One call, with the tool's own input schema |
| What the agent can do to your browser | Drive it: open pages, click, run scripts | Call the tools pages chose to offer |
| Tabs opening and closing | Ask again with `list_webmcp_tools` | The tool list updates itself (`tools/list_changed`) |
| Runtime | Node.js via npx | One 0.5 MB binary, no runtime |

## Setup

1. **Enable WebMCP**: open `chrome://flags/#enable-webmcp-testing`, choose **Enabled** and relaunch. In Brave the page is `brave://flags/#enable-webmcp-testing`; choose **Enabled**, not "Default (Enabled)", which leaves it off.
2. **Allow remote debugging**: open `chrome://inspect/#remote-debugging` (Brave: `brave://inspect/#remote-debugging`) and allow it. The browser may ask you to approve the first connection.
3. **Build** (stable Rust): `cargo build --release`, then put `target/release/webmcp-bridge` on your `PATH`. CI also builds macOS and Linux binaries.
4. **Check**: open a page that registers WebMCP tools and run

   ```bash
   webmcp-bridge doctor --url tldraw.com
   ```

   It lists your browser profiles, connects, and prints the tools it would serve, with the fix for anything missing.

## Connect your agent

**Claude Code**

```bash
claude mcp add webmcp -- webmcp-bridge --url tldraw.com
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.webmcp]
command = "webmcp-bridge"
args = ["--url", "tldraw.com"]
```

**Zed** (`settings.json`)

```json
"context_servers": {
  "webmcp": { "command": "webmcp-bridge", "args": ["--url", "tldraw.com"] }
}
```

## Options

| Option | Meaning |
|---|---|
| `--url <text>` | Only pages whose URL contains `<text>` contribute tools. Repeatable. Without it, every http(s) page does. |
| `--user-data-dir <path>` | The browser profile folder to attach to. By default the first running Chrome, Brave, Edge or Chromium with remote debugging allowed. |
| `--ws-endpoint <url>` | Attach to `ws://127.0.0.1:<port>/devtools/browser/<id>` directly. |
| `--timeout <seconds>` | Longest a tool call may run (default 120). |

When two tabs register the same tool name, each gets a short tab suffix (`search_a1b2c3`).

## How it works

- Reads the browser's `DevToolsActivePort` file, opens the browser-level DevTools WebSocket on localhost, and follows tabs with `Target.setDiscoverTargets`.
- Attaches to each matching tab and calls `WebMCP.enable`; the browser reports tools as they are added and removed (`WebMCP.toolsAdded` / `toolsRemoved`).
- A tool call becomes `WebMCP.invokeTool` on the tool's frame; the result comes back as `WebMCP.toolResponded`. Cancelling a call in your agent sends `WebMCP.cancelInvocation`.
- Reconnects on its own when the browser restarts.
- Tools the page marks as debugging-only are left out. The page's `readOnly`, `consequential` and `untrustedContent` hints become MCP's `readOnlyHint`, `destructiveHint` and `openWorldHint`.

It is one binary with one dependency (`serde_json`): no async runtime, a minimal WebSocket client for the local DevTools socket, plain threads. Messages from the browser are capped at 64 MB, and each page at 128 tools.

## Performance

Measured end to end against Chrome 154 on an M-series Mac: **about 0.22 ms per tool call**, from the MCP request through the browser and the page's `execute` function and back (200 calls, `tests/real_browser.rs`). Release binary: about 0.5 MB.

## Security

- **Your agent only gets what pages offer**: the bridge exposes page tools and nothing that drives the browser.
- **But the bridge itself uses remote debugging**, which gives any local program that can read your profile folder the same access. Allow it only on a machine you trust.
- **Use `--url`** so only sites you trust can put tools in front of your agent. Tool descriptions and results come from the page; treat them as untrusted input.
- The connection is localhost only. The bridge never listens on a port.

## Limitations

- WebMCP is experimental and behind a flag in Chrome and Brave; its DevTools domain may change.
- macOS and Linux. Windows is untested.
- MCP over stdio only.

## Tests

```bash
cargo test                                   # unit tests
WEBMCP_CHROME="/path/to/chrome" cargo test   # plus the real-browser end-to-end test
```

The end-to-end test starts a throwaway headless Chrome profile, serves a page that registers tools, and checks listing, calling, errors and tool-list updates on reload. CI runs it on Linux and macOS.

## License

MIT
