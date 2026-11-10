//! MCP-Server über stdio zum Testen und Ausprobieren: `can-mcp-echo`.

use std::io::{BufRead, Write};

use serde_json::Value;

fn main() -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(message) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        if let Some(reply) = can_tools::mcp::echo::handle(&message) {
            // Eine Benachrichtigung vorweg: Clients müssen sie überspringen können.
            writeln!(
                out,
                "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{{}}}}"
            )?;
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}
