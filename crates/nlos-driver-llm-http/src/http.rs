//! Pure safe Rust bounded `HTTP/1.1` client over `tokio::net::TcpStream`.
//!
//! Supports `http://<host>:<port>/<path>` URL parsing, `GET` and `POST`
//! requests with custom headers and `tokio::time::timeout` deadline control,
//! case-insensitive response header lookup, and all three `HTTP/1.1` body
//! framing modes (`Transfer-Encoding: chunked`, `Content-Length`, and EOF close).

use std::fmt::Write as _;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::LlmDriverError;

/// Default per-request `HTTP/1.1` timeout (`60_000` ms).
pub const DEFAULT_HTTP_TIMEOUT_MS: u64 = 60_000;
/// Maximum allowed `HTTP/1.1` response header block size (`64 KiB`).
pub const MAX_HTTP_HEADER_BYTES: usize = 64 * 1024;
/// Default maximum allowed `HTTP/1.1` response body size (`16 MiB`).
pub const DEFAULT_MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Maximum length of a single chunked transfer-encoding size line.
const MAX_CHUNK_LINE_BYTES: usize = 1024;

/// Supported `HTTP/1.1` request verbs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
}

impl HttpMethod {
    /// Wire verb string (`"GET"` or `"POST"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// Parsed `http://` endpoint URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpUrl {
    /// Dialable host (without IPv6 brackets).
    pub host: String,
    /// TCP port (default `80`).
    pub port: u16,
    /// Request target path + optional query string (always begins with `'/'`).
    pub path_and_query: String,
}

impl HttpUrl {
    /// Parses an `http://<host>[:<port>][/<path>[?<query>]]` URL.
    ///
    /// # Errors
    /// Fails closed on non-`http://` schemes, empty hosts, invalid ports, or
    /// `CR`/`LF` control characters.
    pub fn parse(raw: &str) -> Result<Self, LlmDriverError> {
        let trimmed = raw.trim();
        if trimmed.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(LlmDriverError::InvalidUrl(
                "URL contains forbidden control characters".to_owned(),
            ));
        }
        let Some(rest) = trimmed
            .strip_prefix("http://")
            .or_else(|| trimmed.strip_prefix("HTTP://"))
        else {
            return Err(LlmDriverError::InvalidUrl(
                "only bounded local http:// endpoints are supported".to_owned(),
            ));
        };

        let (authority, path_and_query) = match rest.find('/') {
            Some(idx) => (&rest[..idx], rest[idx..].to_owned()),
            None => match rest.find('?') {
                Some(q_idx) => (&rest[..q_idx], format!("/{}", &rest[q_idx..])),
                None => (rest, "/".to_owned()),
            },
        };

        if authority.is_empty() {
            return Err(LlmDriverError::InvalidUrl(
                "missing URL host authority".to_owned(),
            ));
        }

        let (host, port) = parse_authority(authority)?;
        if host.is_empty() {
            return Err(LlmDriverError::InvalidUrl("empty URL host".to_owned()));
        }
        if port == 0 {
            return Err(LlmDriverError::InvalidUrl(
                "TCP port 0 is not dialable".to_owned(),
            ));
        }

        Ok(Self {
            host,
            port,
            path_and_query,
        })
    }

    /// Formats the `Host` header value per RFC 9112.
    #[must_use]
    pub fn host_header_value(&self) -> String {
        let formatted_host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == 80 {
            formatted_host
        } else {
            format!("{formatted_host}:{}", self.port)
        }
    }
}

fn parse_authority(authority: &str) -> Result<(String, u16), LlmDriverError> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some(end_bracket) = after_bracket.find(']') else {
            return Err(LlmDriverError::InvalidUrl(
                "unclosed IPv6 bracket in URL authority".to_owned(),
            ));
        };
        let host = after_bracket[..end_bracket].to_owned();
        let remainder = &after_bracket[end_bracket + 1..];
        if remainder.is_empty() {
            return Ok((host, 80));
        }
        let Some(port_str) = remainder.strip_prefix(':') else {
            return Err(LlmDriverError::InvalidUrl(
                "unexpected characters after IPv6 bracket".to_owned(),
            ));
        };
        let port = port_str
            .parse::<u16>()
            .map_err(|_| LlmDriverError::InvalidUrl("invalid TCP port in URL".to_owned()))?;
        Ok((host, port))
    } else if let Some((host_part, port_part)) = authority.rsplit_once(':') {
        if host_part.contains(':') {
            return Err(LlmDriverError::InvalidUrl(
                "IPv6 literal in URL must be enclosed in brackets".to_owned(),
            ));
        }
        let port = port_part
            .parse::<u16>()
            .map_err(|_| LlmDriverError::InvalidUrl("invalid TCP port in URL".to_owned()))?;
        Ok((host_part.to_owned(), port))
    } else {
        Ok((authority.to_owned(), 80))
    }
}

/// Outbound `HTTP/1.1` request specification.
#[derive(Clone, Debug)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Decoded `HTTP/1.1` response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    pub status_code: u16,
    pub reason_phrase: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Looks up the first matching response header case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Parses the `Retry-After` integer seconds header when present.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        self.header("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
    }

    /// Decodes the response body as `UTF-8`.
    ///
    /// # Errors
    /// Returns [`LlmDriverError::HttpTransport`] if the response body is not valid `UTF-8`.
    pub fn body_utf8(&self) -> Result<&str, LlmDriverError> {
        std::str::from_utf8(&self.body).map_err(|_| {
            LlmDriverError::HttpTransport("HTTP response body is not valid UTF-8".to_owned())
        })
    }
}

/// Bounded `HTTP/1.1` client backed by `tokio::net::TcpStream`.
#[derive(Clone, Copy, Debug)]
pub struct HttpClient {
    timeout: Duration,
    max_body_bytes: usize,
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new(
            Duration::from_millis(DEFAULT_HTTP_TIMEOUT_MS),
            DEFAULT_MAX_HTTP_BODY_BYTES,
        )
    }
}

impl HttpClient {
    /// Creates an `HTTP/1.1` client with explicit timeout and response body cap.
    #[must_use]
    pub const fn new(timeout: Duration, max_body_bytes: usize) -> Self {
        Self {
            timeout,
            max_body_bytes,
        }
    }

    /// Creates an `HTTP/1.1` client with a custom timeout and the default 16 MiB body cap.
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self::new(timeout, DEFAULT_MAX_HTTP_BODY_BYTES)
    }

    /// Configured per-request timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Executes an `HTTP/1.1` `GET` request.
    ///
    /// # Errors
    /// Fails on invalid URL/headers, timeout, TCP transport failure, or malformed wire response.
    pub async fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, LlmDriverError> {
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            body: Vec::new(),
        };
        self.send(&req).await
    }

    /// Executes an `HTTP/1.1` `POST` request with a `JSON` body.
    ///
    /// # Errors
    /// Fails on invalid URL/headers, timeout, TCP transport failure, or malformed wire response.
    pub async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        json_body: &[u8],
    ) -> Result<HttpResponse, LlmDriverError> {
        let mut all_headers = Vec::with_capacity(headers.len() + 2);
        let has_content_type = headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
        if !has_content_type {
            all_headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        for (k, v) in headers {
            all_headers.push(((*k).to_owned(), (*v).to_owned()));
        }
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: url.to_owned(),
            headers: all_headers,
            body: json_body.to_vec(),
        };
        self.send(&req).await
    }

    /// Sends a generic [`HttpRequest`] within `self.timeout`.
    ///
    /// # Errors
    /// Fails closed on header injection attempts, timeout, TCP failure, or malformed framing.
    pub async fn send(&self, request: &HttpRequest) -> Result<HttpResponse, LlmDriverError> {
        let parsed_url = HttpUrl::parse(&request.url)?;
        let wire_head = encode_request_head(request, &parsed_url)?;
        let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);

        tokio::time::timeout(self.timeout, async {
            let mut stream = TcpStream::connect((parsed_url.host.as_str(), parsed_url.port))
                .await
                .map_err(|err| {
                    LlmDriverError::HttpTransport(format!(
                        "TCP connect to {}:{} failed: {err}",
                        parsed_url.host, parsed_url.port
                    ))
                })?;
            let _ = stream.set_nodelay(true);
            stream
                .write_all(wire_head.as_bytes())
                .await
                .map_err(|err| {
                    LlmDriverError::HttpTransport(format!(
                        "HTTP request header write failed: {err}"
                    ))
                })?;
            if !request.body.is_empty() {
                stream.write_all(&request.body).await.map_err(|err| {
                    LlmDriverError::HttpTransport(format!("HTTP request body write failed: {err}"))
                })?;
            }
            stream.flush().await.map_err(|err| {
                LlmDriverError::HttpTransport(format!("HTTP request flush failed: {err}"))
            })?;

            read_http_response(stream, self.max_body_bytes).await
        })
        .await
        .map_err(|_| LlmDriverError::HttpTimeout { timeout_ms })?
    }
}

fn validate_header_token(name: &str, value: &str) -> Result<(), LlmDriverError> {
    if name.is_empty()
        || name
            .bytes()
            .any(|b| matches!(b, b'\r' | b'\n' | b':' | b' ' | 0))
    {
        return Err(LlmDriverError::HttpTransport(
            "invalid HTTP request header name".to_owned(),
        ));
    }
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
        return Err(LlmDriverError::HttpTransport(
            "HTTP request header value contains forbidden CR/LF/NUL bytes".to_owned(),
        ));
    }
    Ok(())
}

fn encode_request_head(req: &HttpRequest, url: &HttpUrl) -> Result<String, LlmDriverError> {
    let mut head = String::with_capacity(256);
    let _ = write!(
        head,
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        req.method.as_str(),
        url.path_and_query,
        url.host_header_value()
    );
    if req.method == HttpMethod::Post || !req.body.is_empty() {
        let _ = write!(head, "Content-Length: {}\r\n", req.body.len());
    }
    for (k, v) in &req.headers {
        if k.eq_ignore_ascii_case("host")
            || k.eq_ignore_ascii_case("connection")
            || k.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        validate_header_token(k, v)?;
        let _ = write!(head, "{k}: {v}\r\n");
    }
    head.push_str("\r\n");
    Ok(head)
}

async fn read_http_response(
    stream: TcpStream,
    max_body_bytes: usize,
) -> Result<HttpResponse, LlmDriverError> {
    let mut reader = BufHttpReader::new(stream);
    let header_bytes = reader.read_header_block().await?;
    let header_text = std::str::from_utf8(&header_bytes).map_err(|_| {
        LlmDriverError::HttpTransport("HTTP response headers are not valid UTF-8".to_owned())
    })?;

    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().ok_or_else(|| {
        LlmDriverError::HttpTransport("missing HTTP response status line".to_owned())
    })?;
    let (status_code, reason_phrase) = parse_status_line(status_line)?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((raw_name, raw_val)) = line.split_once(':') else {
            return Err(LlmDriverError::HttpTransport(
                "malformed HTTP response header line".to_owned(),
            ));
        };
        headers.push((raw_name.trim().to_owned(), raw_val.trim().to_owned()));
    }

    let is_bodyless = (100..200).contains(&status_code) || status_code == 204 || status_code == 304;
    let is_chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding")
            && v.split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("chunked"))
    });
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| {
            v.trim().parse::<usize>().map_err(|_| {
                LlmDriverError::HttpTransport("invalid Content-Length header value".to_owned())
            })
        })
        .transpose()?;

    let body = if is_bodyless {
        Vec::new()
    } else if is_chunked {
        reader.read_chunked_body(max_body_bytes).await?
    } else if let Some(len) = content_length {
        if len > max_body_bytes {
            return Err(LlmDriverError::HttpTransport(format!(
                "Content-Length {len} exceeds maximum allowed {max_body_bytes} bytes"
            )));
        }
        reader.read_exact_bytes(len).await?
    } else {
        reader.read_to_eof(max_body_bytes).await?
    };

    Ok(HttpResponse {
        status_code,
        reason_phrase,
        headers,
        body,
    })
}

fn parse_status_line(line: &str) -> Result<(u16, String), LlmDriverError> {
    let trimmed = line.trim();
    if !trimmed.starts_with("HTTP/1.1 ") && !trimmed.starts_with("HTTP/1.0 ") {
        return Err(LlmDriverError::HttpTransport(
            "unsupported or malformed HTTP status line".to_owned(),
        ));
    }
    let after_proto = trimmed[9..].trim_start();
    if after_proto.len() < 3 {
        return Err(LlmDriverError::HttpTransport(
            "truncated HTTP status code".to_owned(),
        ));
    }
    let code_str = &after_proto[..3];
    let status_code = code_str
        .parse::<u16>()
        .map_err(|_| LlmDriverError::HttpTransport("non-numeric HTTP status code".to_owned()))?;
    let reason = after_proto[3..].trim().to_owned();
    Ok((status_code, reason))
}

struct BufHttpReader {
    stream: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

impl BufHttpReader {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(4096),
            pos: 0,
        }
    }

    async fn read_more(&mut self) -> Result<usize, LlmDriverError> {
        if self.pos > 0 && self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        let mut chunk = [0_u8; 4096];
        let n = self.stream.read(&mut chunk).await.map_err(|err| {
            LlmDriverError::HttpTransport(format!("HTTP socket read failure: {err}"))
        })?;
        if n > 0 {
            self.buf.extend_from_slice(&chunk[..n]);
        }
        Ok(n)
    }

    async fn read_header_block(&mut self) -> Result<Vec<u8>, LlmDriverError> {
        loop {
            if let Some(rel_idx) = find_subslice(&self.buf[self.pos..], b"\r\n\r\n") {
                let end = self.pos + rel_idx;
                let out = self.buf[self.pos..end].to_vec();
                self.pos = end + 4;
                return Ok(out);
            }
            if self.buf.len().saturating_sub(self.pos) > MAX_HTTP_HEADER_BYTES {
                return Err(LlmDriverError::HttpTransport(
                    "HTTP response headers exceeded MAX_HTTP_HEADER_BYTES".to_owned(),
                ));
            }
            let n = self.read_more().await?;
            if n == 0 {
                return Err(LlmDriverError::HttpTransport(
                    "unexpected EOF before end of HTTP response headers".to_owned(),
                ));
            }
        }
    }

    async fn read_line_crlf(&mut self, max_len: usize) -> Result<String, LlmDriverError> {
        loop {
            if let Some(rel_idx) = find_subslice(&self.buf[self.pos..], b"\r\n") {
                if rel_idx > max_len {
                    return Err(LlmDriverError::HttpTransport(
                        "HTTP chunk line exceeded max_len".to_owned(),
                    ));
                }
                let end = self.pos + rel_idx;
                let line = std::str::from_utf8(&self.buf[self.pos..end])
                    .map_err(|_| {
                        LlmDriverError::HttpTransport(
                            "HTTP chunk header is not valid UTF-8".to_owned(),
                        )
                    })?
                    .to_owned();
                self.pos = end + 2;
                return Ok(line);
            }
            if self.buf.len().saturating_sub(self.pos) > max_len {
                return Err(LlmDriverError::HttpTransport(
                    "HTTP chunk line exceeded max_len without CRLF".to_owned(),
                ));
            }
            let n = self.read_more().await?;
            if n == 0 {
                return Err(LlmDriverError::HttpTransport(
                    "unexpected EOF while reading chunked transfer line".to_owned(),
                ));
            }
        }
    }

    async fn read_exact_bytes(&mut self, total_len: usize) -> Result<Vec<u8>, LlmDriverError> {
        let mut out = Vec::with_capacity(total_len.min(64 * 1024));
        while out.len() < total_len {
            let avail = self.buf.len().saturating_sub(self.pos);
            if avail > 0 {
                let take = avail.min(total_len - out.len());
                out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
                self.pos += take;
            } else {
                let n = self.read_more().await?;
                if n == 0 {
                    return Err(LlmDriverError::HttpTransport(
                        "unexpected EOF before Content-Length/chunk bytes completed".to_owned(),
                    ));
                }
            }
        }
        Ok(out)
    }

    async fn read_chunked_body(
        &mut self,
        max_body_bytes: usize,
    ) -> Result<Vec<u8>, LlmDriverError> {
        let mut body = Vec::new();
        loop {
            let size_line = self.read_line_crlf(MAX_CHUNK_LINE_BYTES).await?;
            let hex_part = size_line.split(';').next().unwrap_or("").trim();
            if hex_part.is_empty() {
                return Err(LlmDriverError::HttpTransport(
                    "empty chunk size in chunked HTTP response".to_owned(),
                ));
            }
            let chunk_size = usize::from_str_radix(hex_part, 16).map_err(|_| {
                LlmDriverError::HttpTransport("invalid hex chunk size in HTTP response".to_owned())
            })?;
            if chunk_size == 0 {
                // Consume optional trailer headers until terminating empty CRLF line.
                loop {
                    let trailer = self.read_line_crlf(MAX_HTTP_HEADER_BYTES).await?;
                    if trailer.is_empty() {
                        break;
                    }
                }
                return Ok(body);
            }
            let next_total = body.len().checked_add(chunk_size).ok_or_else(|| {
                LlmDriverError::HttpTransport("chunked body size overflow".to_owned())
            })?;
            if next_total > max_body_bytes {
                return Err(LlmDriverError::HttpTransport(format!(
                    "chunked HTTP body exceeded maximum {max_body_bytes} bytes"
                )));
            }
            let chunk_bytes = self.read_exact_bytes(chunk_size).await?;
            body.extend_from_slice(&chunk_bytes);
            let crlf = self.read_exact_bytes(2).await?;
            if crlf != b"\r\n" {
                return Err(LlmDriverError::HttpTransport(
                    "missing CRLF after chunk payload".to_owned(),
                ));
            }
        }
    }

    async fn read_to_eof(&mut self, max_body_bytes: usize) -> Result<Vec<u8>, LlmDriverError> {
        let mut out = Vec::new();
        let avail = self.buf.len().saturating_sub(self.pos);
        if avail > 0 {
            if avail > max_body_bytes {
                return Err(LlmDriverError::HttpTransport(
                    "EOF-framed HTTP body exceeded maximum bytes".to_owned(),
                ));
            }
            out.extend_from_slice(&self.buf[self.pos..]);
            self.buf.clear();
            self.pos = 0;
        }
        let mut chunk = [0_u8; 4096];
        loop {
            let n = self.stream.read(&mut chunk).await.map_err(|err| {
                LlmDriverError::HttpTransport(format!("HTTP EOF body read failure: {err}"))
            })?;
            if n == 0 {
                break;
            }
            if out.len().saturating_add(n) > max_body_bytes {
                return Err(LlmDriverError::HttpTransport(
                    "EOF-framed HTTP body exceeded maximum bytes".to_owned(),
                ));
            }
            out.extend_from_slice(&chunk[..n]);
        }
        Ok(out)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::HttpUrl;

    #[test]
    fn parses_valid_http_urls_and_rejects_https_or_crlf() {
        let u = HttpUrl::parse("http://127.0.0.1:8317/api/coding/paas/v4/chat/completions")
            .expect("valid local URL");
        assert_eq!(u.host, "127.0.0.1");
        assert_eq!(u.port, 8317);
        assert_eq!(u.path_and_query, "/api/coding/paas/v4/chat/completions");
        assert_eq!(u.host_header_value(), "127.0.0.1:8317");

        let ipv6 = HttpUrl::parse("http://[::1]:8317/__pool__/status").expect("valid IPv6 URL");
        assert_eq!(ipv6.host, "::1");
        assert_eq!(ipv6.port, 8317);
        assert_eq!(ipv6.host_header_value(), "[::1]:8317");

        assert!(HttpUrl::parse("https://open.bigmodel.cn/api/paas/v4").is_err());
        assert!(HttpUrl::parse("http://127.0.0.1:0/test").is_err());
        assert!(HttpUrl::parse("http://127.0.0.1:8317/test\r\nX-Injected: 1").is_err());
    }
}
