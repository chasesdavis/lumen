//! MCP stdio server entry point.
//!
//! Speaks the Model Context Protocol over stdin/stdout with newline-delimited
//! JSON-RPC 2.0 (the framing every mainstream MCP client uses for stdio).
//! This is the agent-native surface Helumi does not have: agents call
//! structured tools instead of poking mounted files through Finder.

use clap::{Parser, ValueEnum};
use lumen_core::index::MetadataIndex;
pub use lumen_mcp_server::session::{AuthConfig, Session};
use lumen_mcp_server::tools::Tools;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::Arc;

// AuthConfig and Session live in lib (session.rs); see module docs there.

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (default).
    Stdio,
    /// Newline-delimited JSON-RPC over TCP; one thread per connection.
    Tcp,
}

#[derive(Debug, Parser)]
#[command(
    name = "lumen-mcp-server",
    version,
    about = "Lumen MCP tool server exposing agent-native drive operations"
)]
struct Args {
    /// Transport mode for serving MCP clients.
    #[arg(short, long, value_enum, default_value_t = Transport::Stdio)]
    transport: Transport,

    /// TCP listen address in --transport tcp mode.
    #[arg(long, default_value = "127.0.0.1:9317")]
    listen: String,

    /// SQLite file to back the metadata index. Defaults to in-memory,
    /// which is fine for stateless MCP hosts; set this to persist tree
    /// data and the content index across restarts.
    #[arg(long)]
    index_path: Option<String>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("lumen_mcp_server=info")
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let index = match &args.index_path {
        Some(path) => MetadataIndex::open(path),
        None => MetadataIndex::open_memory(),
    };
    let index = match index {
        Ok(ix) => ix,
        Err(e) => {
            eprintln!("failed to open metadata index: {}", e);
            std::process::exit(1);
        }
    };
    let tools = Tools::with_index(index);
    let auth_config = AuthConfig::from_env();

    // TCP mode exists for remote agents and non-stdio hosts; default stays
    // stdio, which every mainstream MCP client uses.
    match args.transport {
        Transport::Tcp => {
            if let Err(e) = serve_tcp(Arc::new(tools), auth_config, args.listen.clone()) {
                eprintln!("tcp server error: {}", e);
                std::process::exit(1);
            }
        }
        Transport::Stdio => serve_stdio(tools, auth_config),
    }
}

fn serve_stdio(tools: Tools, auth_config: AuthConfig) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    // One session for the whole stdio lifetime: authorize once, then all
    // subsequent tool calls in this process stream stay authorized.
    let mut session = Session::default();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = handle_line(&tools, &auth_config, &mut session, &line);
        if reply.is_null() {
            continue;
        }
        match serde_json::to_string(&reply) {
            Ok(s) => {
                let _ = writeln!(out, "{}", s);
                let _ = out.flush();
            }
            Err(e) => {
                let fallback = error_response(Value::Null, -32603, &format!("encode error: {}", e));
                let _ = writeln!(out, "{}", fallback);
                let _ = out.flush();
            }
        }
    }
}

/// Newline-delimited JSON-RPC over TCP - same framing as the stdio transport,
/// so any MCP TCP client can point at the socket. Each connection is served
/// on its own thread with a cloned Arc<Tools>; the in-memory index per
/// process means all connections share one metadata view.
fn serve_tcp(tools: Arc<Tools>, auth_config: AuthConfig, addr: String) -> std::io::Result<()> {
    let listener = std::net::TcpListener::bind(&addr)?;
    tracing::info!("MCP server listening on tcp://{addr}");
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let tools = tools.clone();
        let auth_config = auth_config.clone();
        std::thread::spawn(move || {
            use std::io::BufReader;
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            tracing::info!("client connected: {peer}");
            let reader_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("clone failed for {peer}: {e}");
                    return;
                }
            };
            let mut reader = BufReader::new(reader_stream);
            let mut writer = stream;
            // One session per connection: authorize once at the start,
            // tool calls stay authorized for the life of the connection.
            let mut session = Session::default();
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: client disconnected
                    Ok(_) => {}
                    Err(_) => break,
                }
                if line.trim().is_empty() {
                    continue;
                }
                let reply = handle_line(&tools, &auth_config, &mut session, &line);
                if reply.is_null() {
                    continue;
                }
                match serde_json::to_string(&reply) {
                    Ok(s) => {
                        if writeln!(writer, "{}", s)
                            .and_then(|_| writer.flush())
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let fallback =
                            error_response(Value::Null, -32603, &format!("encode error: {}", e));
                        let _ = writeln!(writer, "{}", fallback);
                        let _ = writer.flush();
                    }
                }
            }
            tracing::info!("client disconnected: {peer}");
        });
    }
    Ok(())
}

fn handle_line(
    tools: &Tools,
    auth_config: &AuthConfig,
    session: &mut Session,
    line: &str,
) -> Value {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return error_response(Value::Null, -32700, &format!("parse error: {}", e)),
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Notifications get no response per JSON-RPC 2.0.
    if id.is_null() {
        return Value::Null;
    }

    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "lumen", "version": env!("CARGO_PKG_VERSION") }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [
                    tool_def("authorize", "Exchange an API access token for an authorized session when the server runs with LUMEN_JWT_SECRET configured"),
                    tool_def("list_files", "List direct children of a folder by parent_id; omit for root"),
                    tool_def("search_files", "Search files by name substring"),
                    tool_def("index_content", "Index a file's first page as plaintext so search_files kind=content can match it"),
                    tool_def("read_file", "Read file content from storage by path or id"),
                    tool_def("file_metadata", "Get full metadata including chunk map and versions"),
                    tool_def("create_file", "Create a new file under a parent folder; requires name, parent_id, optional content (storage-backed mode only)"),
                    tool_def("replace_file", "Replace a file's full contents by id, bumping content and metadata version counters (storage-backed mode only)"),
                    tool_def("delete_file", "Remove an item from the metadata index and commit the tombstone immediately; chunks remain until garbage collection"),
                    tool_def("gc_run", "Sweep unreferenced encrypted chunks from storage and report reclaimed keys and bytes (storage plus commit-log mode only)"),
                ]
            }
        }),
        "authorize" => {
            // The only pre-auth method besides initialize/tools-list.
            // Validates an API access token (JWT signed with the shared
            // LUMEN_JWT_SECRET) and marks this connection authorized.
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let token = params
                .get("token")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            match auth_config.authorize(session, &token) {
                Ok(()) => {
                    json!({"jsonrpc":"2.0","id":id,"result":{"authorized":true,"message":"session authorized; tool calls are now enabled"}})
                }
                Err(why) => error_response(id, -32001, &why),
            }
        }
        "tools/call" => {
            if !auth_config.allows_calls(session) {
                return error_response(
                    id,
                    -32001,
                    "not authorized: call the 'authorize' tool with a valid access token first",
                );
            }
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools.dispatch(&name, args) {
                Ok(result) => {
                    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":serde_json::to_string_pretty(&result).unwrap_or_default()}]}})
                }
                Err(e) => error_response(id, -32000, &format!("tool error: {}", e)),
            }
        }
        _ => error_response(id, -32601, &format!("method not found: {}", method)),
    }
}

fn tool_def(name: &str, desc: &str) -> Value {
    json!({"name": name, "description": desc})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
