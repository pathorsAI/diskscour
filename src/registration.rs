//! Whether DiskScour's MCP server is wired into an agent, and who is on it.
//!
//! The server is stdio: the client spawns `diskscour mcp` as a child process
//! and talks over pipes, so there is no URL or port to show. What the user
//! needs instead is (a) is it registered, and does the registered binary match
//! this build, and (b) how many agent sessions are talking to it right now.
//! Both are answered from local state only — the client's config file and the
//! process table — so asking is cheap and never touches the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything the status line, `diskscour status` and `ds_status` show.
#[derive(Debug, Clone)]
pub struct Status {
    /// The binary answering this question — what a fresh registration should point at.
    pub this_binary: PathBuf,
    pub this_version: &'static str,
    pub registrations: Vec<Registration>,
    /// Live `diskscour mcp` processes, i.e. connected agent sessions.
    pub sessions: Vec<Session>,
}

/// One entry in an agent's MCP config that points at a diskscour binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub client: &'static str,
    /// Where the entry lives: `user`, or `local · <project>` for a per-project entry.
    pub scope: String,
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Points at a diskscour of this version.
    Ok,
    /// The registered path no longer exists.
    Missing,
    /// The entry doesn't launch the server (`mcp` is not among its args).
    NotServer,
    /// A different diskscour build; carries its version string.
    Version(String),
}

impl Health {
    pub fn describe(&self, this_version: &str) -> String {
        match self {
            Health::Ok => "ok".into(),
            Health::Missing => "binary not found".into(),
            Health::NotServer => "entry does not run `diskscour mcp`".into(),
            Health::Version(v) => format!("registered v{v}, this is v{this_version}"),
        }
    }
}

/// A running MCP server process and the client that launched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub pid: i32,
    /// File name of the parent process, e.g. `claude`.
    pub client: String,
}

/// Traffic-light summary for a status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Off,
}

impl Status {
    pub fn collect() -> Status {
        let this_binary = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .unwrap_or_else(|_| PathBuf::from("diskscour"));
        let mut registrations = claude_code_registrations();
        for r in &mut registrations {
            r.health = health_of(r, &this_binary);
        }
        Status {
            this_binary,
            this_version: VERSION,
            registrations,
            sessions: sessions_from_ps(&ps_output(), crate::live::exe_of_pid),
        }
    }

    /// The command that registers this very binary with Claude Code.
    pub fn add_command(&self) -> String {
        format!(
            "claude mcp add diskscour -- {} mcp",
            shell_quote(&self.this_binary.to_string_lossy())
        )
    }

    pub fn level(&self) -> Level {
        if self.registrations.is_empty() {
            Level::Off
        } else if self.registrations.iter().all(|r| r.health == Health::Ok) {
            Level::Ok
        } else {
            Level::Warn
        }
    }

    /// One short phrase for the status bar.
    pub fn headline(&self) -> String {
        match self.level() {
            Level::Off => "MCP · not registered".into(),
            Level::Warn => {
                let r = self
                    .registrations
                    .iter()
                    .find(|r| r.health != Health::Ok)
                    .expect("warn implies an unhealthy registration");
                format!("MCP · {}", r.health.describe(self.this_version))
            }
            Level::Ok => match self.sessions.len() {
                0 => "MCP · registered · no sessions".into(),
                1 => "MCP · 1 session".into(),
                n => format!("MCP · {n} sessions"),
            },
        }
    }

    /// Sessions grouped by client, e.g. `claude ×8`.
    pub fn sessions_by_client(&self) -> String {
        let mut by: BTreeMap<&str, usize> = BTreeMap::new();
        for s in &self.sessions {
            *by.entry(s.client.as_str()).or_default() += 1;
        }
        by.iter()
            .map(|(c, n)| format!("{c} ×{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "transport": "stdio",
            "server_version": self.this_version,
            "server_binary": self.this_binary.to_string_lossy(),
            "registered": !self.registrations.is_empty(),
            "registrations": self.registrations.iter().map(|r| json!({
                "client": r.client,
                "scope": r.scope,
                "name": r.name,
                "command": r.command.to_string_lossy(),
                "args": r.args,
                "health": match &r.health {
                    Health::Ok => "ok",
                    Health::Missing => "missing",
                    Health::NotServer => "not_server",
                    Health::Version(_) => "version_mismatch",
                },
                "detail": r.health.describe(self.this_version),
            })).collect::<Vec<_>>(),
            "sessions": self.sessions.len(),
            "sessions_by_client": self.sessions.iter().fold(
                serde_json::Map::new(),
                |mut m, s| {
                    let n = m.get(&s.client).and_then(Value::as_u64).unwrap_or(0);
                    m.insert(s.client.clone(), json!(n + 1));
                    m
                },
            ),
            "session_pids": self.sessions.iter().map(|s| s.pid).collect::<Vec<_>>(),
            "add_command": self.add_command(),
        })
    }
}

// ---- registrations ----------------------------------------------------------

/// Claude Code keeps user-scope servers under `mcpServers` and per-project
/// ones under `projects.<path>.mcpServers`, both in `~/.claude.json`. Project
/// scope is a `.mcp.json` next to the code; only the current directory's is
/// checked, since the rest can't be enumerated.
fn claude_code_registrations() -> Vec<Registration> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let path = Path::new(&home).join(".claude.json");
        if let Some(v) = read_json(&path) {
            out.extend(registrations_in(&v, "user"));
            if let Some(projects) = v.get("projects").and_then(Value::as_object) {
                for (dir, pv) in projects {
                    out.extend(registrations_in(pv, &format!("local · {dir}")));
                }
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(v) = read_json(&cwd.join(".mcp.json"))
    {
        out.extend(registrations_in(
            &v,
            &format!("project · {}", cwd.display()),
        ));
    }
    out
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Entries under `mcpServers` in `v` whose command is a diskscour binary.
fn registrations_in(v: &Value, scope: &str) -> Vec<Registration> {
    let Some(servers) = v.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, s)| {
            let command = PathBuf::from(s.get("command").and_then(Value::as_str)?);
            let is_ours = command
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|f| f.starts_with("diskscour"));
            if !is_ours {
                return None;
            }
            let args = s
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(Registration {
                client: "Claude Code",
                scope: scope.to_string(),
                name: name.clone(),
                command,
                args,
                health: Health::Ok,
            })
        })
        .collect()
}

fn health_of(r: &Registration, this_binary: &Path) -> Health {
    if !r.args.iter().any(|a| a == "mcp") {
        return Health::NotServer;
    }
    // A bare name is resolved through PATH by the client; try the same.
    let resolved = if r.command.is_absolute() {
        r.command.clone()
    } else {
        match which(&r.command) {
            Some(p) => p,
            None => return Health::Missing,
        }
    };
    let Ok(canon) = resolved.canonicalize() else {
        return Health::Missing;
    };
    if canon == this_binary {
        return Health::Ok;
    }
    match version_of(&canon) {
        Some(v) if v == VERSION => Health::Ok,
        Some(v) => Health::Version(v),
        None => Health::Missing,
    }
}

fn which(name: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Ask another diskscour binary its version. `--version` returns at once
/// without touching the index, so this is safe to call from a status refresh.
fn version_of(bin: &Path) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim().strip_prefix("diskscour ").map(str::to_string)
}

// ---- sessions ---------------------------------------------------------------

#[cfg(target_os = "macos")]
fn ps_output() -> String {
    Command::new("ps")
        .args(["-axo", "pid=,ppid=,args="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn ps_output() -> String {
    String::new()
}

/// Every `diskscour mcp` process in a `ps -axo pid=,ppid=,args=` listing,
/// labelled with the name of the process that spawned it.
fn sessions_from_ps(ps: &str, exe_of: impl Fn(i32) -> Option<PathBuf>) -> Vec<Session> {
    struct Row<'a> {
        pid: i32,
        ppid: i32,
        args: &'a str,
    }
    let rows: Vec<Row> = ps
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let args = it.next().map(|first| {
                let start = line.find(first).unwrap_or(0);
                line[start..].trim_end()
            })?;
            Some(Row { pid, ppid, args })
        })
        .collect();

    // Prefer the kernel's answer for the parent's executable; the `ps` column
    // is only a fallback, since a path with spaces splits wrong there.
    let name_of = |pid: i32| -> String {
        exe_of(pid)
            .or_else(|| {
                rows.iter()
                    .find(|r| r.pid == pid)
                    .and_then(|r| r.args.split_whitespace().next())
                    .map(PathBuf::from)
            })
            .map(|exe| client_name(&exe))
            .unwrap_or_else(|| format!("pid {pid}"))
    };

    rows.iter()
        .filter(|r| {
            // The executable may sit in a path with spaces (an .app bundle), so
            // look for the name anywhere before the final `mcp` argument.
            let mut words = r.args.split_whitespace();
            let last = words.next_back();
            last == Some("mcp") && r.args.contains("diskscour")
        })
        .map(|r| Session {
            pid: r.pid,
            client: name_of(r.ppid),
        })
        .collect()
}

/// A readable name for a client from its executable path. Launchers hide the
/// product name behind bundle plumbing (`Claude.app/Contents/MacOS/claude`) or
/// a version directory (`~/.local/share/claude/versions/2.1.259`), so walk up
/// from the file name to the first component that reads like a product.
fn client_name(exe: &Path) -> String {
    const PLUMBING: &[&str] = &[
        "versions",
        "bin",
        "MacOS",
        "Contents",
        "Frameworks",
        "Resources",
        "Helpers",
    ];
    let looks_like_version = |s: &str| s.starts_with(|c: char| c.is_ascii_digit());
    exe.components()
        .rev()
        .filter_map(|c| c.as_os_str().to_str())
        .find(|c| !PLUMBING.contains(c) && !looks_like_version(c))
        .map(|c| c.strip_suffix(".app").unwrap_or(c).to_string())
        .unwrap_or_else(|| exe.to_string_lossy().into_owned())
}

fn shell_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+~".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_diskscour_entries_and_ignores_others() {
        let v = json!({
            "mcpServers": {
                "diskscour": {"type": "stdio", "command": "/opt/bin/diskscour", "args": ["mcp"]},
                "other": {"command": "/usr/bin/node", "args": ["server.js"]},
            }
        });
        let regs = registrations_in(&v, "user");
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].name, "diskscour");
        assert_eq!(regs[0].command, PathBuf::from("/opt/bin/diskscour"));
        assert_eq!(regs[0].args, vec!["mcp".to_string()]);
        assert_eq!(regs[0].scope, "user");
    }

    #[test]
    fn missing_mcp_arg_is_not_a_server() {
        let r = Registration {
            client: "Claude Code",
            scope: "user".into(),
            name: "diskscour".into(),
            command: PathBuf::from("/nonexistent/diskscour"),
            args: vec![],
            health: Health::Ok,
        };
        assert_eq!(health_of(&r, Path::new("/x")), Health::NotServer);
        let r = Registration {
            args: vec!["mcp".into()],
            ..r
        };
        assert_eq!(health_of(&r, Path::new("/x")), Health::Missing);
    }

    #[test]
    fn counts_mcp_processes_and_names_their_parent() {
        let ps = "\
    1     0 /sbin/launchd
  500     1 /Users/me/.local/bin/claude
  501   500 /Users/me/.local/bin/diskscour mcp
  502   500 /Applications/Disk Scour.app/Contents/MacOS/diskscour mcp
  503     1 /Users/me/.local/bin/diskscour
  504     1 /Users/me/.local/bin/diskscour scan /tmp
";
        let s = sessions_from_ps(ps, |_| None);
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0],
            Session {
                pid: 501,
                client: "claude".into()
            }
        );
        assert_eq!(s[1].pid, 502);
        assert_eq!(s[1].client, "claude");
    }

    #[test]
    fn headline_reflects_level() {
        let mut st = Status {
            this_binary: PathBuf::from("/x/diskscour"),
            this_version: VERSION,
            registrations: vec![],
            sessions: vec![],
        };
        assert_eq!(st.level(), Level::Off);
        assert_eq!(st.headline(), "MCP · not registered");
        st.registrations.push(Registration {
            client: "Claude Code",
            scope: "user".into(),
            name: "diskscour".into(),
            command: PathBuf::from("/x/diskscour"),
            args: vec!["mcp".into()],
            health: Health::Ok,
        });
        assert_eq!(st.headline(), "MCP · registered · no sessions");
        st.sessions.push(Session {
            pid: 1,
            client: "claude".into(),
        });
        st.sessions.push(Session {
            pid: 2,
            client: "claude".into(),
        });
        assert_eq!(st.headline(), "MCP · 2 sessions");
        assert_eq!(st.sessions_by_client(), "claude ×2");
        st.registrations[0].health = Health::Version("0.1.0".into());
        assert_eq!(st.level(), Level::Warn);
        assert!(st.headline().contains("registered v0.1.0"));
    }

    #[test]
    fn client_names_see_through_bundles_and_version_dirs() {
        let n = |p: &str| client_name(Path::new(p));
        assert_eq!(
            n("/Users/me/.local/share/claude/versions/2.1.259"),
            "claude"
        );
        assert_eq!(
            n(
                "/Users/me/Library/Application Support/Claude/claude-code/2.1.258/claude.app/Contents/MacOS/claude"
            ),
            "claude"
        );
        assert_eq!(
            n("/Applications/Cursor.app/Contents/MacOS/Cursor"),
            "Cursor"
        );
        assert_eq!(n("/usr/local/bin/node"), "node");
    }

    #[test]
    fn add_command_quotes_spaces() {
        let st = Status {
            this_binary: PathBuf::from("/Applications/Disk Scour.app/Contents/MacOS/diskscour"),
            this_version: VERSION,
            registrations: vec![],
            sessions: vec![],
        };
        assert_eq!(
            st.add_command(),
            "claude mcp add diskscour -- '/Applications/Disk Scour.app/Contents/MacOS/diskscour' mcp"
        );
    }
}
