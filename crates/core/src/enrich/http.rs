//! A minimal client for an OpenAI-compatible chat-completions endpoint.
//!
//! `http://` URLs (a model on this machine or the local network) are spoken
//! to directly over HTTP/1.1 with the standard library. `https://` URLs are
//! sent through the system `curl` (8.3 or later), which reads the API key
//! from its environment through `--variable`, so the key never appears in a
//! command line, a file or an error message. Only the configured URL is
//! ever contacted; nothing in a file or a response can change it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::{ModelClient, ModelSettings, MAX_RESPONSE_BYTES};

pub struct HttpModelClient {
    url: String,
    api_key_env: Option<String>,
    api_key: Option<String>,
    timeout: Duration,
}

impl HttpModelClient {
    /// Reads the API key from the environment variable the config names.
    pub fn new(s: &ModelSettings) -> Result<Self> {
        let api_key = match &s.api_key_env {
            Some(var) => Some(std::env::var(var).map_err(|_| {
                crate::error::invalid(format!("the environment variable {var} (named by [model] api_key_env) is not set"))
            })?),
            None => None,
        };
        Ok(Self {
            url: s.url.clone(),
            api_key_env: s.api_key_env.clone(),
            api_key,
            timeout: Duration::from_secs(s.timeout_secs),
        })
    }

    fn post_http(&self, rest: &str, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let host_port = if authority.rsplit_once(':').is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit())) && !authority.ends_with(']') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        let addr = host_port
            .to_socket_addrs()
            .with_context(|| format!("resolving {authority}"))?
            .next()
            .ok_or_else(|| anyhow!("{authority} has no address"))?;
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout).with_context(|| format!("connecting to {authority}"))?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nAccept: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\nUser-Agent: mirrorself\r\n",
            body.len()
        );
        if let Some(key) = &self.api_key {
            head.push_str(&format!("Authorization: Bearer {key}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()?;
        let mut raw = Vec::new();
        stream
            .take((MAX_RESPONSE_BYTES + 64 * 1024) as u64)
            .read_to_end(&mut raw)
            .context("reading the model's response")?;
        parse_http_response(&raw)
    }

    fn post_curl(&self, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        let mut cmd = Command::new("curl");
        cmd.args(["--silent", "--show-error", "--proto", "=https", "--request", "POST"])
            .args(["--max-time", &self.timeout.as_secs().to_string()])
            .args(["--max-filesize", &MAX_RESPONSE_BYTES.to_string()])
            .args(["--header", "Content-Type: application/json", "--header", "Accept: application/json"]);
        if let Some(var) = &self.api_key_env {
            // curl expands {{VAR}} from its own environment: the key is not
            // in the arguments.
            cmd.args(["--variable", &format!("%{var}"), "--expand-header", &format!("Authorization: Bearer {{{{{var}}}}}")]);
        }
        cmd.args(["--data-binary", "@-", "--write-out", "\n%{http_code}", "--url", &self.url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("running curl for an https:// model endpoint (is curl installed?)")?;
        child.stdin.take().expect("piped").write_all(body)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("curl failed: {}", crate::importers::files::clean_line(&err, 300));
        }
        let stdout = out.stdout;
        let split = stdout.iter().rposition(|&b| b == b'\n').ok_or_else(|| anyhow!("curl gave no status"))?;
        let status: u16 = std::str::from_utf8(&stdout[split + 1..])?.trim().parse().context("curl gave no status")?;
        Ok((status, stdout[..split].to_vec()))
    }
}

impl ModelClient for HttpModelClient {
    fn chat(&self, request: &Value) -> Result<Vec<u8>> {
        let body = serde_json::to_vec(request)?;
        let (status, resp) = if let Some(rest) = self.url.strip_prefix("http://") {
            self.post_http(rest, &body)?
        } else if self.url.starts_with("https://") {
            self.post_curl(&body)?
        } else {
            bail!("unsupported endpoint scheme");
        };
        if !(200..300).contains(&status) {
            let snippet = crate::importers::files::clean_line(&String::from_utf8_lossy(&resp[..resp.len().min(300)]), 300);
            bail!("the model endpoint answered HTTP {status}: {snippet}");
        }
        if resp.len() > MAX_RESPONSE_BYTES {
            bail!("the model's response is larger than {MAX_RESPONSE_BYTES} bytes");
        }
        Ok(resp)
    }
}

/// Status and body of a raw HTTP/1.1 response, de-chunked if needed.
pub fn parse_http_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("the model endpoint sent an incomplete HTTP response"))?;
    let head = std::str::from_utf8(&raw[..end]).context("the response headers are not text")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("bad HTTP status line"))?;
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if k == "content-length" {
                length = v.parse().ok();
            }
        }
    }
    let body = &raw[end + 4..];
    if chunked {
        return Ok((status, dechunk(body)?));
    }
    let body = match length {
        Some(n) if n <= body.len() => &body[..n],
        Some(_) => bail!("the model's response was cut off"),
        None => body,
    };
    Ok((status, body.to_vec()))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").ok_or_else(|| anyhow!("bad chunked encoding"))?;
        let size_str = std::str::from_utf8(&body[..line_end])?.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).context("bad chunk size")?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 || out.len() + size > MAX_RESPONSE_BYTES {
            bail!("the model's response was cut off or too large");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked_responses() {
        let (s, b) = parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloXX").unwrap();
        assert_eq!((s, b.as_slice()), (200, &b"hello"[..]));
        let (s, b) =
            parse_http_response(b"HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nhel\r\n2;x=y\r\nlo\r\n0\r\n\r\n").unwrap();
        assert_eq!((s, b.as_slice()), (201, &b"hello"[..]));
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort").is_err());
        assert!(parse_http_response(b"garbage").is_err());
    }
}
