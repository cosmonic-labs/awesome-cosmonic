//! HTTP/1.1 on the wire, kept close to what Node's `llhttp` sees: header names keep their case
//! and order, values are raw bytes, and duplicates stay separate, so the guest's `rawHeaders`
//! match a Node server's.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Node's default `maxHeaderSize`.
pub const MAX_HEADER_SIZE: usize = 16 * 1024;
const MAX_HEADERS: usize = 2000;

pub type Header = (String, Vec<u8>);

pub struct RequestHead {
    pub method: String,
    pub target: String,
    /// The minor version: 0 for HTTP/1.0, 1 for HTTP/1.1.
    pub minor: u8,
    pub headers: Vec<Header>,
}

pub struct ResponseHead {
    pub status: u16,
    pub reason: String,
    pub minor: u8,
    pub headers: Vec<Header>,
}

/// A connection's unread bytes, kept across messages.
pub struct Reader<R> {
    inner: R,
    buffer: Vec<u8>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub fn header<'a>(headers: &'a [Header], name: &str) -> Option<&'a [u8]> {
    headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_slice())
}

fn has_token(headers: &[Header], name: &str, token: &str) -> bool {
    headers.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name)).any(|(_, value)| {
        String::from_utf8_lossy(value).split(',').any(|part| part.trim().eq_ignore_ascii_case(token))
    })
}

/// How a message's body is delimited.
#[derive(Debug, PartialEq, Eq)]
pub enum Framing {
    Length(u64),
    Chunked,
    /// Read until the peer closes; only responses can be framed this way.
    Close,
}

pub fn request_framing(headers: &[Header]) -> io::Result<Framing> {
    if has_token(headers, "transfer-encoding", "chunked") {
        return Ok(Framing::Chunked);
    }
    match header(headers, "content-length") {
        Some(value) => std::str::from_utf8(value)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .map(Framing::Length)
            .ok_or_else(|| invalid("Invalid Content-Length")),
        None => Ok(Framing::Length(0)),
    }
}

pub fn response_framing(method: &str, head: &ResponseHead) -> io::Result<Framing> {
    if method.eq_ignore_ascii_case("HEAD") || (100..200).contains(&head.status) || matches!(head.status, 204 | 304) {
        return Ok(Framing::Length(0));
    }
    if has_token(&head.headers, "transfer-encoding", "chunked") {
        return Ok(Framing::Chunked);
    }
    match header(&head.headers, "content-length") {
        Some(_) => request_framing(&head.headers),
        None => Ok(Framing::Close),
    }
}

/// Whether the connection stays open after this exchange, by HTTP/1.x's rules.
pub fn keep_alive(minor: u8, headers: &[Header]) -> bool {
    if has_token(headers, "connection", "close") {
        return false;
    }
    minor >= 1 || has_token(headers, "connection", "keep-alive")
}

pub fn wants_continue(headers: &[Header]) -> bool {
    header(headers, "expect").is_some_and(|value| value.eq_ignore_ascii_case(b"100-continue"))
}

pub fn is_upgrade(headers: &[Header]) -> bool {
    header(headers, "upgrade").is_some() && has_token(headers, "connection", "upgrade")
}

fn collect(parsed: &[httparse::Header<'_>]) -> Vec<Header> {
    parsed.iter().map(|header| (header.name.to_owned(), header.value.to_vec())).collect()
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, buffer: Vec::new() }
    }

    /// Bytes read past the end of what has been parsed: the start of an upgraded protocol.
    pub fn take_buffered(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buffer)
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Read more bytes; `false` at the end of input.
    async fn fill(&mut self) -> io::Result<bool> {
        let mut chunk = [0u8; 16 * 1024];
        let read = self.inner.read(&mut chunk).await?;
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(read > 0)
    }

    /// The next request head, or `None` when the peer closes between requests.
    pub async fn request_head(&mut self, max: usize) -> io::Result<Option<RequestHead>> {
        loop {
            // Blank lines between requests are allowed and skipped.
            while self.buffer.starts_with(b"\r\n") {
                self.buffer.drain(..2);
            }
            if !self.buffer.is_empty() {
                let mut headers = vec![httparse::EMPTY_HEADER; MAX_HEADERS];
                let mut request = httparse::Request::new(&mut headers);
                match request.parse(&self.buffer).map_err(|error| invalid(&error.to_string()))? {
                    httparse::Status::Complete(length) => {
                        let head = RequestHead {
                            method: request.method.unwrap_or("GET").to_owned(),
                            target: request.path.unwrap_or("/").to_owned(),
                            minor: request.version.unwrap_or(1),
                            headers: collect(request.headers),
                        };
                        self.buffer.drain(..length);
                        return Ok(Some(head));
                    }
                    httparse::Status::Partial if self.buffer.len() > max => {
                        return Err(invalid("Header overflow"));
                    }
                    httparse::Status::Partial => {}
                }
            }
            if !self.fill().await? {
                return if self.buffer.is_empty() { Ok(None) } else { Err(invalid("Parse Error")) };
            }
        }
    }

    pub async fn response_head(&mut self, max: usize) -> io::Result<ResponseHead> {
        loop {
            if !self.buffer.is_empty() {
                let mut headers = vec![httparse::EMPTY_HEADER; MAX_HEADERS];
                let mut response = httparse::Response::new(&mut headers);
                match response.parse(&self.buffer).map_err(|error| invalid(&error.to_string()))? {
                    httparse::Status::Complete(length) => {
                        let head = ResponseHead {
                            status: response.code.unwrap_or(0),
                            reason: response.reason.unwrap_or("").to_owned(),
                            minor: response.version.unwrap_or(1),
                            headers: collect(response.headers),
                        };
                        self.buffer.drain(..length);
                        return Ok(head);
                    }
                    httparse::Status::Partial if self.buffer.len() > max => {
                        return Err(invalid("Header overflow"));
                    }
                    httparse::Status::Partial => {}
                }
            }
            if !self.fill().await? {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "socket hang up"));
            }
        }
    }

    async fn exact(&mut self, length: usize) -> io::Result<Vec<u8>> {
        while self.buffer.len() < length {
            if !self.fill().await? {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "aborted"));
            }
        }
        Ok(self.buffer.drain(..length).collect())
    }

    async fn line(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if let Some(end) = self.buffer.windows(2).position(|window| window == b"\r\n") {
                let line = self.buffer.drain(..end + 2).take(end).collect();
                return Ok(line);
            }
            if self.buffer.len() > MAX_HEADER_SIZE {
                return Err(invalid("Invalid chunk size"));
            }
            if !self.fill().await? {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "aborted"));
            }
        }
    }

    /// The next piece of a body: `None` once it is complete.
    pub async fn body_chunk(&mut self, framing: &mut Framing) -> io::Result<Option<Vec<u8>>> {
        match framing {
            Framing::Length(0) => Ok(None),
            Framing::Length(remaining) => {
                if self.buffer.is_empty() && !self.fill().await? {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "aborted"));
                }
                let take = (*remaining).min(self.buffer.len() as u64) as usize;
                *remaining -= take as u64;
                Ok(Some(self.buffer.drain(..take).collect()))
            }
            Framing::Chunked => {
                let line = self.line().await?;
                let size = String::from_utf8_lossy(&line);
                let size = size.split(';').next().unwrap_or("").trim();
                let size = usize::from_str_radix(size, 16).map_err(|_| invalid("Invalid character in chunk size"))?;
                if size == 0 {
                    // Trailers, up to the blank line that ends the message.
                    while !self.line().await?.is_empty() {}
                    *framing = Framing::Length(0);
                    return Ok(None);
                }
                let data = self.exact(size).await?;
                if !self.line().await?.is_empty() {
                    return Err(invalid("Invalid chunk"));
                }
                Ok(Some(data))
            }
            Framing::Close => {
                if self.buffer.is_empty() && !self.fill().await? {
                    *framing = Framing::Length(0);
                    return Ok(None);
                }
                Ok(Some(std::mem::take(&mut self.buffer)))
            }
        }
    }

    pub async fn body(&mut self, mut framing: Framing) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = self.body_chunk(&mut framing).await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// `Date` as Node's `utcDate()` writes it.
pub fn http_date(at: std::time::SystemTime) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let seconds = at.duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs()) as i64;
    let days = seconds.div_euclid(86_400);
    let clock = seconds.rem_euclid(86_400);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(month - 1) as usize],
        clock / 3600,
        clock % 3600 / 60,
        clock % 60
    )
}

pub fn write_head(out: &mut Vec<u8>, start: &str, headers: &[Header]) {
    out.extend_from_slice(start.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
}

pub async fn write_chunk<W: AsyncWrite + Unpin>(writer: &mut W, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let mut frame = format!("{:x}\r\n", data.len()).into_bytes();
    frame.extend_from_slice(data);
    frame.extend_from_slice(b"\r\n");
    writer.write_all(&frame).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_keep_raw_headers_and_read_chunked_bodies() {
        let wire: &[u8] =
            b"POST /items HTTP/1.1\r\nHost: x\r\nX-Twice: a\r\nx-twice: b\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let mut reader = Reader::new(wire);
        let head = reader.request_head(MAX_HEADER_SIZE).await.unwrap().unwrap();
        assert_eq!((head.method.as_str(), head.target.as_str(), head.minor), ("POST", "/items", 1));
        assert_eq!(head.headers[1], ("X-Twice".to_owned(), b"a".to_vec()));
        assert_eq!(head.headers[2], ("x-twice".to_owned(), b"b".to_vec()));
        let framing = request_framing(&head.headers).unwrap();
        assert_eq!(reader.body(framing).await.unwrap(), b"hello");
        assert!(reader.request_head(MAX_HEADER_SIZE).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn responses_without_length_read_to_close() {
        let wire: &[u8] = b"HTTP/1.0 200 Fine\r\nContent-Type: text/plain\r\n\r\nall of it";
        let mut reader = Reader::new(wire);
        let head = reader.response_head(MAX_HEADER_SIZE).await.unwrap();
        assert_eq!((head.status, head.reason.as_str(), head.minor), (200, "Fine", 0));
        let framing = response_framing("GET", &head).unwrap();
        assert_eq!(reader.body(framing).await.unwrap(), b"all of it");
    }

    #[test]
    fn dates_are_imf_fixdate() {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(784_111_777);
        assert_eq!(http_date(at), "Sun, 06 Nov 1994 08:49:37 GMT");
    }
}
