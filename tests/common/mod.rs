//! A tiny MCP client that drives the bridge binary over stdio, for the integration tests.
#![allow(dead_code)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub struct Mcp {
    stdin: ChildStdin,
    lines: Receiver<Value>,
    next_id: u64,
    pub notifications: Vec<Value>,
}

impl Mcp {
    pub fn start(args: &[&str]) -> (Mcp, Child) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_webmcp-bridge"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start webmcp-bridge");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        (
            Mcp {
                stdin,
                lines,
                next_id: 1,
                notifications: Vec::new(),
            },
            child,
        )
    }

    pub fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Sends a request and returns its response; notifications seen meanwhile are kept.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("response in time");
            let msg = self.lines.recv_timeout(left).expect("bridge answered");
            if msg["id"] == id {
                return msg;
            }
            if msg.get("method").is_some() {
                self.notifications.push(msg);
            }
        }
    }

    pub fn initialize(&mut self) {
        let r = self.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }));
        assert_eq!(
            r["result"]["capabilities"]["tools"]["listChanged"], true,
            "{r}"
        );
        self.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }

    pub fn tool_names(&mut self) -> Vec<String> {
        let r = self.request("tools/list", json!({}));
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    pub fn wait_notification(&mut self, method: &str, within: Duration) -> bool {
        if self.notifications.iter().any(|n| n["method"] == method) {
            return true;
        }
        let deadline = Instant::now() + within;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.lines.recv_timeout(left) {
                Ok(msg) if msg["method"] == method => return true,
                Ok(msg) => self.notifications.push(msg),
                Err(_) => return false,
            }
        }
        false
    }
}
