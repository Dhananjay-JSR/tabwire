//! The bridge: keeps a live list of the WebMCP tools registered by matching browser tabs, and serves
//! them to an MCP client over stdio as ordinary tools.
//!
//! Threads: the manager (browser connection, tab attach, tool list), the CDP reader (inside `cdp`),
//! the stdin loop, and one short-lived thread per tool call so calls and cancellations never block it.

use crate::cdp::{self, Cdp, Incoming};
use crate::discover;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Tool results the browser sent before anyone waited for them (bounded, see `route_response`).
const MAX_EARLY_RESULTS: usize = 256;
/// Per-page cap so one page cannot flood the client's context with tools.
const MAX_TOOLS_PER_PAGE: usize = 128;

pub struct Config {
    pub ws_endpoint: Option<String>,
    pub user_data_dir: Option<PathBuf>,
    /// Substrings a page URL must contain to contribute tools; empty means every http(s) page.
    pub urls: Vec<String>,
    pub call_timeout: Duration,
}

impl Config {
    pub fn wants(&self, url: &str) -> bool {
        (url.starts_with("https://") || url.starts_with("http://"))
            && (self.urls.is_empty() || self.urls.iter().any(|u| url.contains(u.as_str())))
    }
}

/// One tool as registered by a page.
#[derive(Clone)]
struct PageTool {
    name: String,
    frame: String,
    session: String,
    target: String,
    description: String,
    schema: Value,
    annotations: Value,
}

#[derive(Default)]
struct State {
    cdp: Option<Arc<Cdp>>,
    tools: Vec<PageTool>,
    /// targetId -> sessionId for tabs we are attached to.
    attached: HashMap<String, String>,
    waiters: HashMap<String, mpsc::Sender<Value>>,
    early: HashMap<String, Value>,
    /// JSON-RPC request id (as text) -> (invocationId, sessionId), for cancellation.
    calls: HashMap<String, (String, String)>,
    ready: bool,
    initialized: bool,
    last_problem: Option<String>,
}

pub struct Bridge {
    cfg: Config,
    state: Mutex<State>,
    ready: Condvar,
    out: Mutex<io::Stdout>,
}

impl Bridge {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Bridge {
            cfg,
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            out: Mutex::new(io::stdout()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, msg: &Value) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }

    fn problem(&self, msg: String) {
        let mut st = self.lock();
        if st.last_problem.as_deref() != Some(msg.as_str()) {
            eprintln!("tabwire: {msg}");
            st.last_problem = Some(msg);
        }
    }

    fn mark_ready(&self) {
        self.lock().ready = true;
        self.ready.notify_all();
    }

    fn tools_changed(&self) {
        if self.lock().initialized {
            self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
        }
    }

    // ------------------------------------------------------------------ browser side

    /// Connects, follows tabs, and reconnects when the browser goes away. Never returns.
    pub fn run_manager(self: &Arc<Self>) {
        loop {
            match self.connect_once() {
                Ok(rx) => {
                    for ev in rx {
                        match ev {
                            Incoming::Event(ev) => self.handle(&ev),
                            Incoming::Closed => break,
                        }
                    }
                    self.disconnected();
                }
                Err(e) => {
                    self.problem(e);
                    self.mark_ready(); // tools/list answers (empty) instead of waiting forever
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    fn connect_once(self: &Arc<Self>) -> Result<Receiver<Incoming>, String> {
        let endpoint = discover::resolve(
            self.cfg.ws_endpoint.as_deref(),
            self.cfg.user_data_dir.as_deref(),
        )?;
        let (cdp, rx) =
            cdp::connect(&endpoint).map_err(|e| format!("cannot connect to {endpoint}: {e}"))?;
        self.lock().cdp = Some(cdp.clone());
        cdp.call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )?;
        {
            let mut st = self.lock();
            st.last_problem = None;
        }
        eprintln!("tabwire: connected to {endpoint}");
        // Existing tabs arrive as targetCreated events right away; settle them before the first list.
        loop {
            match rx.recv_timeout(Duration::from_millis(150)) {
                Ok(Incoming::Event(ev)) => self.handle(&ev),
                Ok(Incoming::Closed) => return Err("the browser closed the connection".into()),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("the browser connection ended".into())
                }
            }
        }
        self.mark_ready();
        self.tools_changed();
        Ok(rx)
    }

    fn disconnected(&self) {
        let had_tools = {
            let mut st = self.lock();
            st.cdp = None;
            st.attached.clear();
            st.waiters.clear(); // pending calls fail at once instead of timing out
            st.calls.clear();
            st.early.clear();
            !std::mem::take(&mut st.tools).is_empty()
        };
        self.problem("the browser connection closed; reconnecting".into());
        if had_tools {
            self.tools_changed();
        }
    }

    fn handle(self: &Arc<Self>, ev: &Value) {
        let params = &ev["params"];
        let session = ev["sessionId"].as_str().unwrap_or_default();
        match ev["method"].as_str().unwrap_or_default() {
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                self.on_target(&params["targetInfo"])
            }
            "Target.targetDestroyed" => {
                self.forget_target(params["targetId"].as_str().unwrap_or_default())
            }
            "Target.detachedFromTarget" => {
                let sid = params["sessionId"].as_str().unwrap_or_default();
                let target = self
                    .lock()
                    .attached
                    .iter()
                    .find(|(_, s)| *s == sid)
                    .map(|(t, _)| t.clone());
                if let Some(t) = target {
                    self.forget_target(&t);
                }
            }
            "WebMCP.toolsAdded" => self.add_tools(session, &params["tools"]),
            "WebMCP.toolsRemoved" => self.remove_tools(session, &params["tools"]),
            "WebMCP.toolResponded" => self.route_response(params),
            _ => {}
        }
    }

    fn on_target(&self, info: &Value) {
        if info["type"] != "page" {
            return;
        }
        let target = info["targetId"].as_str().unwrap_or_default().to_string();
        let wanted = self.cfg.wants(info["url"].as_str().unwrap_or_default());
        let attached = self.lock().attached.contains_key(&target);
        if wanted && !attached {
            self.attach(&target);
        } else if !wanted && attached {
            if let (Some(cdp), Some(sid)) = {
                let st = self.lock();
                (st.cdp.clone(), st.attached.get(&target).cloned())
            } {
                let _ = cdp.call("Target.detachFromTarget", json!({ "sessionId": sid }), None);
            }
            self.forget_target(&target);
        }
    }

    fn attach(&self, target: &str) {
        let Some(cdp) = self.lock().cdp.clone() else {
            return;
        };
        let session = match cdp.call(
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
            None,
        ) {
            Ok(r) => r["sessionId"].as_str().unwrap_or_default().to_string(),
            Err(e) => return self.problem(e),
        };
        if session.is_empty() {
            return;
        }
        self.lock()
            .attached
            .insert(target.to_string(), session.clone());
        // The browser answers with toolsAdded for every tool the page has already registered.
        if let Err(e) = cdp.call("WebMCP.enable", json!({}), Some(&session)) {
            self.problem(format!(
                "{e}. Is WebMCP enabled? Set chrome://flags/#enable-webmcp-testing (brave://flags in Brave) to Enabled and relaunch"
            ));
        }
    }

    fn forget_target(&self, target: &str) {
        let changed = {
            let mut st = self.lock();
            st.attached.remove(target);
            let before = st.tools.len();
            st.tools.retain(|t| t.target != target);
            st.tools.len() != before
        };
        if changed {
            self.tools_changed();
        }
    }

    fn add_tools(&self, session: &str, tools: &Value) {
        let Some(list) = tools.as_array() else { return };
        {
            let mut st = self.lock();
            let Some(target) = st
                .attached
                .iter()
                .find(|(_, s)| *s == session)
                .map(|(t, _)| t.clone())
            else {
                return;
            };
            for t in list {
                if t["annotations"]["debugging"] == true {
                    continue; // developer tooling, not for agents
                }
                let name = t["name"].as_str().unwrap_or_default();
                let frame = t["frameId"].as_str().unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                st.tools
                    .retain(|x| !(x.session == session && x.frame == frame && x.name == name));
                if st.tools.iter().filter(|x| x.session == session).count() >= MAX_TOOLS_PER_PAGE {
                    continue;
                }
                st.tools.push(PageTool {
                    name: name.to_string(),
                    frame: frame.to_string(),
                    session: session.to_string(),
                    target: target.clone(),
                    description: t["description"].as_str().unwrap_or_default().to_string(),
                    schema: t["inputSchema"].clone(),
                    annotations: t["annotations"].clone(),
                });
            }
        }
        self.tools_changed();
    }

    fn remove_tools(&self, session: &str, tools: &Value) {
        let Some(list) = tools.as_array() else { return };
        {
            let mut st = self.lock();
            for t in list {
                let (name, frame) = (
                    t["name"].as_str().unwrap_or_default(),
                    t["frameId"].as_str().unwrap_or_default(),
                );
                st.tools
                    .retain(|x| !(x.session == session && x.frame == frame && x.name == name));
            }
        }
        self.tools_changed();
    }

    /// Delivers a tool's result to the call waiting for it, or keeps it until that call registers.
    fn route_response(&self, params: &Value) {
        let Some(inv) = params["invocationId"].as_str() else {
            return;
        };
        let mut st = self.lock();
        if let Some(tx) = st.waiters.remove(inv) {
            let _ = tx.send(params.clone());
        } else {
            if st.early.len() >= MAX_EARLY_RESULTS {
                st.early.clear();
            }
            st.early.insert(inv.to_string(), params.clone());
        }
    }

    // ------------------------------------------------------------------ MCP side

    /// Reads JSON-RPC from stdin until it closes.
    pub fn serve(self: &Arc<Self>) {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(msg) => self.dispatch(msg),
                Err(_) => self.send(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })),
            }
        }
    }

    fn dispatch(self: &Arc<Self>, msg: Value) {
        let id = msg.get("id").cloned();
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let reply = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
        match method.as_str() {
            "initialize" => {
                let asked = msg["params"]["protocolVersion"].as_str().unwrap_or_default();
                let version = SUPPORTED_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSION);
                self.send(&reply(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "tabwire", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "These tools are registered by web pages open in the user's browser (WebMCP). They change as tabs open and close."
                })));
            }
            "notifications/initialized" => self.lock().initialized = true,
            "ping" => self.send(&reply(json!({}))),
            "tools/list" => {
                self.wait_ready(Duration::from_secs(3));
                self.send(&reply(json!({ "tools": self.list() })));
            }
            "tools/call" => {
                let me = self.clone();
                let params = msg["params"].clone();
                thread::spawn(move || {
                    let result = me.call(&id, &params);
                    me.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                });
            }
            "notifications/cancelled" => self.cancel(&msg["params"]["requestId"]),
            _ if id.is_some() => self.send(&json!({
                "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unknown method {method}") }
            })),
            _ => {} // unknown notification
        }
    }

    fn wait_ready(&self, limit: Duration) {
        let st = self.lock();
        let _ = self.ready.wait_timeout_while(st, limit, |s| !s.ready);
    }

    /// MCP tool definitions, with names unique across tabs.
    fn list(&self) -> Vec<Value> {
        let st = self.lock();
        let names = exposed_names(&st.tools);
        st.tools
            .iter()
            .zip(names)
            .map(|(t, name)| {
                let mut schema = if t.schema.is_object() {
                    t.schema.clone()
                } else {
                    json!({})
                };
                if schema.get("type").is_none() {
                    schema["type"] = json!("object");
                }
                let a = &t.annotations;
                json!({
                    "name": name,
                    "description": t.description,
                    "inputSchema": schema,
                    "annotations": {
                        "readOnlyHint": a["readOnly"] == true,
                        "destructiveHint": a["consequential"] == true,
                        "openWorldHint": a["untrustedContent"] == true
                    }
                })
            })
            .collect()
    }

    fn call(&self, id: &Option<Value>, params: &Value) -> Value {
        match self.invoke(id, params) {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
            Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
        }
    }

    fn invoke(&self, id: &Option<Value>, params: &Value) -> Result<String, String> {
        let wanted = params["name"]
            .as_str()
            .ok_or("tools/call needs a tool name")?;
        let input = match &params["arguments"] {
            Value::Null => json!({}),
            v @ Value::Object(_) => v.clone(),
            _ => return Err("arguments must be an object".into()),
        };
        let (tool, cdp) = {
            let st = self.lock();
            let names = exposed_names(&st.tools);
            let i = names.iter().position(|n| n == wanted).ok_or_else(|| {
                format!("no tool named {wanted} is open in the browser right now")
            })?;
            (
                st.tools[i].clone(),
                st.cdp.clone().ok_or("not connected to the browser")?,
            )
        };
        let started = cdp.call(
            "WebMCP.invokeTool",
            json!({ "frameId": tool.frame, "toolName": tool.name, "input": input }),
            Some(&tool.session),
        )?;
        let inv = started["invocationId"]
            .as_str()
            .ok_or("the browser returned no invocationId")?
            .to_string();
        let key = id.as_ref().map(Value::to_string).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let early = {
            let mut st = self.lock();
            st.calls
                .insert(key.clone(), (inv.clone(), tool.session.clone()));
            let early = st.early.remove(&inv);
            if early.is_none() {
                st.waiters.insert(inv.clone(), tx);
            }
            early
        };
        let response = match early {
            Some(r) => Ok(r),
            None => rx.recv_timeout(self.cfg.call_timeout),
        };
        {
            let mut st = self.lock();
            st.calls.remove(&key);
            st.waiters.remove(&inv);
        }
        let r = match response {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                let _ = cdp.call(
                    "WebMCP.cancelInvocation",
                    json!({ "invocationId": inv }),
                    Some(&tool.session),
                );
                return Err(format!(
                    "{} did not answer within {} s",
                    tool.name,
                    self.cfg.call_timeout.as_secs()
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("the browser connection closed during the call".into())
            }
        };
        tool_result(&r)
    }

    fn cancel(&self, request_id: &Value) {
        let found = {
            let st = self.lock();
            st.calls
                .get(&request_id.to_string())
                .cloned()
                .zip(st.cdp.clone())
        };
        if let Some(((inv, session), cdp)) = found {
            thread::spawn(move || {
                let _ = cdp.call(
                    "WebMCP.cancelInvocation",
                    json!({ "invocationId": inv }),
                    Some(&session),
                );
            });
        }
    }
}

/// Turns a `WebMCP.toolResponded` event into the tool's text, or its error.
fn tool_result(r: &Value) -> Result<String, String> {
    let status = r["status"].as_str().unwrap_or_default();
    let ok = status.eq_ignore_ascii_case("success") || status.eq_ignore_ascii_case("completed");
    let output = match &r["output"] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    };
    if ok {
        return Ok(output);
    }
    // Chrome sends errorText "" for a thrown error; the message is the exception's first line
    // (the rest is a stack trace with the page's URLs).
    let nonempty = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let detail = nonempty(&r["errorText"])
        .or_else(|| {
            nonempty(&r["exception"]["description"])
                .map(|d| d.lines().next().unwrap_or_default().to_string())
        })
        .unwrap_or(output);
    Err(if detail.is_empty() {
        format!("the tool ended with status {status}")
    } else {
        detail
    })
}

/// MCP names: the page's tool name (sanitised to the characters MCP clients accept); when two tabs
/// register the same name, each gets a short suffix from its tab id.
fn exposed_names(tools: &[PageTool]) -> Vec<String> {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(56)
            .collect()
    };
    tools
        .iter()
        .map(|t| {
            let dup = tools.iter().filter(|o| o.name == t.name).count() > 1;
            if dup {
                format!(
                    "{}_{}",
                    clean(&t.name),
                    t.target.chars().take(6).collect::<String>().to_lowercase()
                )
            } else {
                clean(&t.name)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, target: &str) -> PageTool {
        PageTool {
            name: name.into(),
            frame: "F".into(),
            session: format!("s-{target}"),
            target: target.into(),
            description: String::new(),
            schema: Value::Null,
            annotations: Value::Null,
        }
    }

    #[test]
    fn names_are_kept_unless_two_tabs_clash() {
        let names = exposed_names(&[
            tool("tldraw_pen", "AAAAAA11"),
            tool("search", "BBBBBB22"),
            tool("search", "CCCCCC33"),
        ]);
        assert_eq!(names, ["tldraw_pen", "search_bbbbbb", "search_cccccc"]);
        assert_eq!(exposed_names(&[tool("do it!", "T")]), ["do_it_"]);
    }

    #[test]
    fn tool_results_map_status_output_and_errors() {
        assert_eq!(
            tool_result(&json!({ "status": "Success", "output": "{\"a\":1}" })),
            Ok("{\"a\":1}".into())
        );
        assert_eq!(
            tool_result(&json!({ "status": "Success", "output": { "a": 1 } })),
            Ok("{\"a\":1}".into())
        );
        assert_eq!(
            tool_result(&json!({ "status": "Error", "errorText": "boom" })),
            Err("boom".into())
        );
        assert_eq!(
            tool_result(&json!({ "status": "Canceled" })),
            Err("the tool ended with status Canceled".into())
        );
        let thrown = json!({ "status": "Error", "errorText": "", "exception": { "description": "Error: nope\n    at execute (http://x/:7:32)" } });
        assert_eq!(tool_result(&thrown), Err("Error: nope".into()));
    }

    #[test]
    fn url_filter_needs_http_and_a_match() {
        let cfg = Config {
            ws_endpoint: None,
            user_data_dir: None,
            urls: vec!["tldraw.com".into()],
            call_timeout: Duration::from_secs(1),
        };
        assert!(cfg.wants("https://www.tldraw.com/f/abc"));
        assert!(!cfg.wants("https://example.com"));
        assert!(!cfg.wants("chrome://newtab"));
        let all = Config {
            urls: vec![],
            ..cfg
        };
        assert!(all.wants("http://localhost:3000"));
    }
}
