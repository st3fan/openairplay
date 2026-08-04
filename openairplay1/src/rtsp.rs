//! Minimal RTSP/1.0 message parsing and serialization, covering the Apple
//! dialect used by AirPlay 1 senders. Requests arrive on a long-lived TCP
//! connection, one after another; responses echo the request's CSeq.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

const MAX_HEADERS: usize = 128;
/// Bodies above this are refused (which drops the connection). Cover art
/// arrives as a `SET_PARAMETER` body of tens to hundreds of KB, so the cap
/// has to sit well above that: at 8 MB it only catches a sender that has
/// gone wrong, never a legitimate payload.
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    fn push(&mut self, name: String, value: String) {
        self.0.push((name, value));
    }
}

/// Read one RTSP request. Returns `Ok(None)` on a clean EOF at a message
/// boundary (client closed the connection).
pub async fn read_request<R>(reader: &mut BufReader<R>) -> io::Result<Option<Request>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let request_line = match read_line(reader).await? {
        None => return Ok(None),
        Some(line) if line.is_empty() => return Ok(None),
        Some(line) => line,
    };

    let mut parts = request_line.splitn(3, ' ');
    let (method, uri, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(u), Some(v)) if !m.is_empty() => (m, u, v),
        _ => {
            return Err(bad_data(format!(
                "malformed request line: {request_line:?}"
            )))
        }
    };
    if !version.starts_with("RTSP/") {
        return Err(bad_data(format!("not an RTSP request: {request_line:?}")));
    }

    let mut headers = Headers::default();
    loop {
        let line = read_line(reader)
            .await?
            .ok_or_else(|| bad_data("EOF inside headers".to_string()))?;
        if line.is_empty() {
            break;
        }
        if headers.0.len() >= MAX_HEADERS {
            return Err(bad_data("too many headers".to_string()));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad_data(format!("malformed header line: {line:?}")))?;
        headers.push(name.trim().to_string(), value.trim().to_string());
    }

    let mut body = Vec::new();
    if let Some(len) = headers.get("Content-Length") {
        let len: usize = len
            .parse()
            .map_err(|_| bad_data(format!("bad Content-Length: {len:?}")))?;
        if len > MAX_BODY {
            return Err(bad_data(format!("body of {len} bytes is too large")));
        }
        body.resize(len, 0);
        reader.read_exact(&mut body).await?;
    }

    Ok(Some(Request {
        method: method.to_string(),
        uri: uri.to_string(),
        headers,
        body,
    }))
}

async fn read_line<R>(reader: &mut BufReader<R>) -> io::Result<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}

fn bad_data(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[derive(Debug)]
pub struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, reason: &'static str) -> Self {
        Response {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn ok() -> Self {
        Response::new(200, "OK")
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub async fn write_to<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let mut out = format!("RTSP/1.0 {} {}\r\n", self.status, self.reason).into_bytes();
        for (name, value) in &self.headers {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        if !self.body.is_empty() {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        writer.write_all(&out).await?;
        writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    async fn parse(input: &[u8]) -> io::Result<Option<Request>> {
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        read_request(&mut reader).await
    }

    #[tokio::test]
    async fn parses_request_with_headers_and_body() {
        let req = parse(
            b"ANNOUNCE rtsp://192.168.1.2/1234 RTSP/1.0\r\n\
              CSeq: 2\r\n\
              Content-Type: application/sdp\r\n\
              Content-Length: 5\r\n\
              \r\n\
              hello",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(req.method, "ANNOUNCE");
        assert_eq!(req.uri, "rtsp://192.168.1.2/1234");
        assert_eq!(req.headers.get("cseq"), Some("2"));
        assert_eq!(req.headers.get("CONTENT-TYPE"), Some("application/sdp"));
        assert_eq!(req.body, b"hello");
    }

    #[tokio::test]
    async fn parses_two_requests_on_one_connection() {
        let input = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\nOPTIONS * RTSP/1.0\r\nCSeq: 2\r\n\r\n";
        let mut reader = BufReader::new(Cursor::new(input.to_vec()));
        let first = read_request(&mut reader).await.unwrap().unwrap();
        let second = read_request(&mut reader).await.unwrap().unwrap();
        assert_eq!(first.headers.get("CSeq"), Some("1"));
        assert_eq!(second.headers.get("CSeq"), Some("2"));
        assert!(read_request(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_returns_none() {
        assert!(parse(b"").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_non_rtsp() {
        assert!(parse(b"GET / HTTP/1.1\r\n\r\n").await.is_err());
        assert!(parse(b"garbage\r\n\r\n").await.is_err());
    }

    #[tokio::test]
    async fn serializes_response() {
        let mut out = Vec::new();
        Response::ok()
            .header("CSeq", "7")
            .header("Public", "OPTIONS")
            .write_to(&mut out)
            .await
            .unwrap();
        assert_eq!(
            out,
            b"RTSP/1.0 200 OK\r\nCSeq: 7\r\nPublic: OPTIONS\r\n\r\n"
        );
    }
}
