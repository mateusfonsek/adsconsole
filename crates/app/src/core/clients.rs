//! Ready-made snippets to connect each AI client to the local MCP. The app never writes
//! to other programs' configuration: it only builds the text for the user to paste.

use adsconsole_core::Platform;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum AiClient {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
    GeminiCli,
    Cursor,
}

#[derive(Debug, Clone, PartialEq, Serialize, specta::Type)]
pub struct Snippet {
    pub text: String,
    /// Translation key saying where to paste the snippet.
    pub where_to_paste: String,
}

/// Mask that replaces the token in the on-screen preview.
pub const MASK: &str = "••••";

pub fn server_name(p: Platform) -> &'static str {
    match p {
        Platform::GoogleAds => "adsconsole-google",
        Platform::MetaAds => "adsconsole-meta",
    }
}

pub fn snippet(client: AiClient, platform: Platform, port: u16, token: &str) -> Snippet {
    let name = server_name(platform);
    let url = format!("http://127.0.0.1:{port}{}", platform.endpoint());
    let bearer = format!("Bearer {token}");
    let (text, place) = match client {
        AiClient::ClaudeCode => (
            format!(
                "claude mcp add --transport http {name} {url} --header \"Authorization: {bearer}\""
            ),
            "terminal",
        ),
        AiClient::GeminiCli => (
            format!(
                "gemini mcp add --scope user --transport http {name} {url} --header \"Authorization: {bearer}\""
            ),
            "terminal",
        ),
        // The Claude Desktop file only accepts local servers: the `mcp-remote` bridge
        // connects the HTTP MCP. The header goes through a variable so the space does not break it.
        AiClient::ClaudeDesktop => (
            servers(
                name,
                Bridge {
                    command: "npx",
                    args: [
                        "-y",
                        "mcp-remote",
                        &url,
                        "--header",
                        "Authorization:${AUTH_HEADER}",
                    ],
                    env: BridgeEnv {
                        auth_header: &bearer,
                    },
                },
            ),
            "claude_desktop",
        ),
        AiClient::Codex => (
            format!(
                "[mcp_servers.{name}]\nurl = \"{url}\"\nhttp_headers = {{ \"Authorization\" = \"{bearer}\" }}"
            ),
            "codex",
        ),
        AiClient::Cursor => (
            servers(
                name,
                Remote {
                    url: &url,
                    headers: Headers {
                        authorization: &bearer,
                    },
                },
            ),
            "cursor",
        ),
    };
    Snippet {
        text,
        where_to_paste: format!("clients.where.{place}"),
    }
}

/// The same snippet with the token replaced by `MASK`, to show on screen.
pub fn masked(s: &Snippet, token: &str) -> String {
    if token.is_empty() {
        return s.text.clone();
    }
    s.text.replace(token, MASK)
}

// Structs instead of `json!`: the key order follows the field order.
#[derive(Serialize)]
struct Bridge<'a> {
    command: &'a str,
    args: [&'a str; 5],
    env: BridgeEnv<'a>,
}

#[derive(Serialize)]
struct BridgeEnv<'a> {
    #[serde(rename = "AUTH_HEADER")]
    auth_header: &'a str,
}

#[derive(Serialize)]
struct Remote<'a> {
    url: &'a str,
    headers: Headers<'a>,
}

#[derive(Serialize)]
struct Headers<'a> {
    #[serde(rename = "Authorization")]
    authorization: &'a str,
}

/// `{"mcpServers": {name: entry}}` with 2-space indentation.
fn servers(name: &str, entry: impl Serialize) -> String {
    #[derive(Serialize)]
    struct Root<'a, E> {
        #[serde(rename = "mcpServers")]
        mcp_servers: std::collections::BTreeMap<&'a str, E>,
    }
    let root = Root {
        mcp_servers: [(name, entry)].into_iter().collect(),
    };
    serde_json::to_string_pretty(&root).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(c: AiClient) -> String {
        snippet(c, Platform::GoogleAds, 7458, "tok").text
    }

    #[test]
    fn claude_code_command() {
        assert_eq!(
            text(AiClient::ClaudeCode),
            "claude mcp add --transport http adsconsole-google http://127.0.0.1:7458/mcp/google-ads --header \"Authorization: Bearer tok\""
        );
    }

    #[test]
    fn claude_desktop_uses_mcp_remote() {
        assert_eq!(
            text(AiClient::ClaudeDesktop),
            r#"{
  "mcpServers": {
    "adsconsole-google": {
      "command": "npx",
      "args": [
        "-y",
        "mcp-remote",
        "http://127.0.0.1:7458/mcp/google-ads",
        "--header",
        "Authorization:${AUTH_HEADER}"
      ],
      "env": {
        "AUTH_HEADER": "Bearer tok"
      }
    }
  }
}"#
        );
    }

    #[test]
    fn codex_toml() {
        assert_eq!(
            text(AiClient::Codex),
            "[mcp_servers.adsconsole-google]\nurl = \"http://127.0.0.1:7458/mcp/google-ads\"\nhttp_headers = { \"Authorization\" = \"Bearer tok\" }"
        );
    }

    #[test]
    fn gemini_command() {
        assert_eq!(
            text(AiClient::GeminiCli),
            "gemini mcp add --scope user --transport http adsconsole-google http://127.0.0.1:7458/mcp/google-ads --header \"Authorization: Bearer tok\""
        );
    }

    #[test]
    fn cursor_json() {
        assert_eq!(
            text(AiClient::Cursor),
            r#"{
  "mcpServers": {
    "adsconsole-google": {
      "url": "http://127.0.0.1:7458/mcp/google-ads",
      "headers": {
        "Authorization": "Bearer tok"
      }
    }
  }
}"#
        );
    }

    #[test]
    fn meta_uses_its_endpoint_and_name() {
        let t = snippet(AiClient::ClaudeCode, Platform::MetaAds, 9000, "tok").text;
        assert!(
            t.contains("adsconsole-meta http://127.0.0.1:9000/mcp/meta-ads"),
            "{t}"
        );
    }

    #[test]
    fn where_to_paste_keys() {
        let w = |c| snippet(c, Platform::GoogleAds, 1, "t").where_to_paste;
        assert_eq!(w(AiClient::ClaudeCode), "clients.where.terminal");
        assert_eq!(w(AiClient::GeminiCli), "clients.where.terminal");
        assert_eq!(w(AiClient::ClaudeDesktop), "clients.where.claude_desktop");
        assert_eq!(w(AiClient::Codex), "clients.where.codex");
        assert_eq!(w(AiClient::Cursor), "clients.where.cursor");
    }

    #[test]
    fn masked_hides_token() {
        for c in [
            AiClient::ClaudeCode,
            AiClient::ClaudeDesktop,
            AiClient::Codex,
            AiClient::GeminiCli,
            AiClient::Cursor,
        ] {
            let token = "s3cr3t-token";
            let s = snippet(c, Platform::GoogleAds, 7458, token);
            let m = masked(&s, token);
            assert!(!m.contains(token), "{m}");
            assert!(m.contains(MASK), "{m}");
        }
    }
}
