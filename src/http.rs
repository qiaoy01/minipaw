use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use rustls_pki_types::ServerName;

pub struct Request<'a> {
    pub method: &'a str,
    pub scheme: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    pub timeout: Duration,
}

pub fn request(req: Request) -> Result<String, String> {
    let addr_iter = (req.host, req.port)
        .to_socket_addrs()
        .map_err(|err| format!("dns: {err}"))?;
    let addr = addr_iter
        .into_iter()
        .next()
        .ok_or_else(|| "dns: no address resolved".to_owned())?;

    let tcp = TcpStream::connect_timeout(&addr, req.timeout)
        .map_err(|err| format!("connect: {err}"))?;
    tcp.set_read_timeout(Some(req.timeout))
        .map_err(|err| format!("set_read_timeout: {err}"))?;
    tcp.set_write_timeout(Some(req.timeout))
        .map_err(|err| format!("set_write_timeout: {err}"))?;

    let head = format_request_head(&req);
    let raw = match req.scheme {
        "https" => transmit_https(req.host, tcp, &head, req.body)?,
        "http" => transmit_plain(tcp, &head, req.body)?,
        other => return Err(format!("unsupported scheme: {other}")),
    };
    parse_response(&raw)
}

fn format_request_head(req: &Request) -> String {
    let mut head = String::new();
    head.push_str(&format!("{} {} HTTP/1.1\r\n", req.method, req.path));
    head.push_str(&format!("Host: {}\r\n", req.host));
    let mut have_content_length = false;
    let mut have_connection = false;
    for (k, v) in req.headers {
        if k.eq_ignore_ascii_case("Content-Length") {
            have_content_length = true;
        }
        if k.eq_ignore_ascii_case("Connection") {
            have_connection = true;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !have_content_length {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    if !have_connection {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    head
}

fn transmit_plain(mut tcp: TcpStream, head: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    tcp.write_all(head.as_bytes())
        .map_err(|err| format!("write headers: {err}"))?;
    if !body.is_empty() {
        tcp.write_all(body)
            .map_err(|err| format!("write body: {err}"))?;
    }
    tcp.flush().map_err(|err| format!("flush: {err}"))?;
    let mut raw = Vec::new();
    tcp.read_to_end(&mut raw)
        .map_err(|err| format!("read: {err}"))?;
    Ok(raw)
}

fn transmit_https(host: &str, tcp: TcpStream, head: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    let config = shared_client_config();
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|err| format!("invalid server name: {err}"))?;
    let conn = ClientConnection::new(config, server_name)
        .map_err(|err| format!("tls handshake init: {err}"))?;
    let mut tls = StreamOwned::new(conn, tcp);

    tls.write_all(head.as_bytes())
        .map_err(|err| format!("write headers: {err}"))?;
    if !body.is_empty() {
        tls.write_all(body)
            .map_err(|err| format!("write body: {err}"))?;
    }
    tls.flush().map_err(|err| format!("flush: {err}"))?;

    let mut raw = Vec::new();
    match tls.read_to_end(&mut raw) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(err) => return Err(format!("read: {err}")),
    }
    Ok(raw)
}

static CLIENT_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

fn shared_client_config() -> Arc<ClientConfig> {
    CLIENT_CONFIG
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn parse_response(raw: &[u8]) -> Result<String, String> {
    let header_end = find_subsequence(raw, b"\r\n\r\n")
        .ok_or_else(|| "http: response had no header terminator".to_owned())?;
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|err| format!("http: header not utf8: {err}"))?;
    let body = &raw[header_end + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or_else(|| "http: missing status line".to_owned())?;
    let mut status_parts = status_line.split(' ');
    let _version = status_parts.next();
    let status_text = status_parts
        .next()
        .ok_or_else(|| "http: status line missing code".to_owned())?;
    let status: u16 = status_text
        .parse()
        .map_err(|err| format!("http: bad status code {status_text:?}: {err}"))?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name_lc = name.trim().to_ascii_lowercase();
            let value_t = value.trim();
            if name_lc == "transfer-encoding" {
                if value_t.split(',').any(|p| p.trim().eq_ignore_ascii_case("chunked")) {
                    chunked = true;
                }
            } else if name_lc == "content-length" {
                content_length = value_t.parse().ok();
            }
        }
    }

    let body_text = if chunked {
        decode_chunked(body)?
    } else if let Some(n) = content_length {
        let n = n.min(body.len());
        std::str::from_utf8(&body[..n])
            .map_err(|err| format!("http: body not utf8: {err}"))?
            .to_owned()
    } else {
        std::str::from_utf8(body)
            .map_err(|err| format!("http: body not utf8: {err}"))?
            .to_owned()
    };

    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {body_text}"));
    }
    Ok(body_text)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(raw: &[u8]) -> Result<String, String> {
    let mut out = Vec::new();
    let mut rest = raw;
    loop {
        let crlf = find_subsequence(rest, b"\r\n")
            .ok_or_else(|| "chunked: missing size CRLF".to_owned())?;
        let size_line = std::str::from_utf8(&rest[..crlf])
            .map_err(|err| format!("chunked: size not utf8: {err}"))?;
        let size_text = size_line.split(';').next().unwrap_or(size_line).trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|err| format!("chunked: bad size {size_text:?}: {err}"))?;
        rest = &rest[crlf + 2..];
        if size == 0 {
            return String::from_utf8(out)
                .map_err(|err| format!("chunked: body not utf8: {err}"))
        }
        if rest.len() < size + 2 {
            return Err("chunked: truncated chunk".to_owned());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_length_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_response(raw).unwrap(), "hello");
    }

    #[test]
    fn parses_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), "hello world");
    }

    #[test]
    fn parses_eof_terminated_body() {
        let raw = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello world";
        assert_eq!(parse_response(raw).unwrap(), "hello world");
    }

    #[test]
    fn surfaces_non_2xx_status() {
        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 5\r\n\r\noops!";
        let err = parse_response(raw).unwrap_err();
        assert!(err.contains("HTTP 400"));
        assert!(err.contains("oops!"));
    }
}
