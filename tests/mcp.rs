//! Drives the real binary over stdio the way an MCP client does.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Client {
    fn spawn(cache_dir: &PathBuf) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_diskscour"))
            .arg("mcp")
            .env("DISKSCOUR_CACHE_DIR", cache_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn diskscour mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Client {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).expect("one JSON-RPC response per line");
        assert_eq!(resp["id"], id);
        resp["result"].clone()
    }

    /// Call a tool and parse its text content as JSON.
    fn tool(&mut self, name: &str, args: Value) -> Value {
        let r = self.call("tools/call", json!({"name": name, "arguments": args}));
        let text = r["content"][0]["text"].as_str().unwrap();
        assert_eq!(r["isError"], false, "{name} failed: {text}");
        serde_json::from_str(text).unwrap()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tmp(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ds_mcp_{}_{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("proj/target/debug")).unwrap();
    fs::write(base.join("proj/target/debug/blob"), vec![0u8; 40_000]).unwrap();
    base
}

#[test]
fn scans_complete_across_calls_and_show_up_in_status() {
    let cache = tmp("cache");
    let first = tmp("first");
    let second = tmp("second");
    let mut mcp = Client::spawn(&cache);

    let init = mcp.call("initialize", json!({}));
    assert_eq!(init["serverInfo"]["name"], "diskscour");

    let done = mcp.tool("ds_scan", json!({"path": first.to_str().unwrap()}));
    assert_eq!(done["status"], "done");
    assert_eq!(done["root"], first.to_str().unwrap());
    assert!(done["total_bytes"].as_u64().unwrap() >= 40_000);

    let early = mcp.tool(
        "ds_scan",
        json!({"path": second.to_str().unwrap(), "wait_seconds": 0}),
    );
    assert!(
        early["status"] == "running" || early["status"] == "done",
        "unexpected status {}",
        early["status"]
    );
    let later = mcp.tool(
        "ds_scan",
        json!({"path": second.to_str().unwrap(), "wait_seconds": 10}),
    );
    assert_eq!(later["status"], "done");
    assert_eq!(later["root"], second.to_str().unwrap());

    let status = mcp.tool("ds_status", json!({}));
    let roots: Vec<&str> = status["indexed_roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["root"].as_str().unwrap())
        .collect();
    assert!(roots.contains(&second.to_str().unwrap()), "{roots:?}");
    assert!(status["running_scans"].is_array());

    drop(mcp);
    for d in [&cache, &first, &second] {
        let _ = fs::remove_dir_all(d);
    }
}
