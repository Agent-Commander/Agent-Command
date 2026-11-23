use std::{
    fmt,
    io::{self, BufRead, Read, Write},
};

pub(crate) const MAX_HEADER_BYTES: usize = 16 * 1024;
pub(crate) const MAX_HEADERS: usize = 64;
pub(crate) const MAX_BODY_BYTES: usize = 1 << 20;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn query_value(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HttpError {
    Closed,
    Malformed(&'static str),
    HeadersTooLarge,
    BodyTooLarge,
    LengthRequired,
    Io(String),
}

impl HttpError {
    pub(crate) fn status(&self) -> u16 {
        match self {
            Self::Closed | Self::Malformed(_) | Self::Io(_) => 400,
            Self::HeadersTooLarge => 431,
            Self::BodyTooLarge => 413,
            Self::LengthRequired => 411,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("connection closed"),
            Self::Malformed(reason) => write!(formatter, "malformed request: {reason}"),
            Self::HeadersTooLarge => formatter.write_str("headers too large"),
            Self::BodyTooLarge => formatter.write_str("body too large"),
            Self::LengthRequired => formatter.write_str("Content-Length required"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<io::Error> for HttpError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

fn read_line(reader: &mut impl BufRead, budget: &mut usize) -> Result<String, HttpError> {
    let mut line = Vec::new();
    let limit = (*budget + 1) as u64;
    let read = reader.take(limit).read_until(b'\n', &mut line)?;
    if read == 0 {
        return Err(HttpError::Closed);
    }
    if read > *budget {
        return Err(HttpError::HeadersTooLarge);
    }
    *budget -= read;
    if !line.ends_with(b"\n") {
        return Err(HttpError::Malformed("unterminated line"));
    }
    let text = String::from_utf8(line).map_err(|_| HttpError::Malformed("non UTF-8 header"))?;
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

pub(crate) fn parse_query(text: &str) -> Result<Vec<(String, String)>, HttpError> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if [key, value].iter().any(|part| {
                !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
            }) {
                return Err(HttpError::Malformed("unsupported query characters"));
            }
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

pub(crate) fn read_request(reader: &mut impl BufRead) -> Result<Request, HttpError> {
    let mut budget = MAX_HEADER_BYTES;
    let start = read_line(reader, &mut budget)?;
    let mut parts = start.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed("request line"));
    };
    if !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return Err(HttpError::Malformed("request target"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader, &mut budget)?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(HttpError::HeadersTooLarge);
        }
        let (name, value) = line.split_once(':').ok_or(HttpError::Malformed("header"))?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let mut request = Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query: parse_query(query)?,
        headers,
        body: Vec::new(),
    };
    if request.header("transfer-encoding").is_some() {
        return Err(HttpError::LengthRequired);
    }
    let length = match request.header("content-length") {
        Some(text) => text
            .parse::<usize>()
            .map_err(|_| HttpError::Malformed("content-length"))?,
        None if request.method == "POST" => return Err(HttpError::LengthRequired),
        None => 0,
    };
    if length > MAX_BODY_BYTES {
        return Err(HttpError::BodyTooLarge);
    }
    request.body = vec![0; length];
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

pub(crate) fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    }
}

pub(crate) fn write_response(
    writer: &mut impl Write,
    status: u16,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        reason(status),
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

pub(crate) fn write_stream_head(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
    )?;
    writer.flush()
}

#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn is_json(&self) -> bool {
        self.header("content-type")
            .is_some_and(|value| value.starts_with("application/json"))
    }
}

pub(crate) fn read_status_and_headers(
    reader: &mut impl BufRead,
) -> Result<(u16, Vec<(String, String)>), HttpError> {
    let mut budget = MAX_HEADER_BYTES;
    let start = read_line(reader, &mut budget)?;
    let status = start
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(HttpError::Malformed("status line"))?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader, &mut budget)?;
        if line.is_empty() {
            return Ok((status, headers));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
}

pub(crate) fn read_response(reader: &mut impl BufRead) -> Result<Response, HttpError> {
    let (status, headers) = read_status_and_headers(reader)?;
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let mut body = Vec::new();
    match length {
        Some(length) if length > MAX_BODY_BYTES * 4 => return Err(HttpError::BodyTooLarge),
        Some(length) => {
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        }
        None => {
            reader
                .take((MAX_BODY_BYTES * 4) as u64)
                .read_to_end(&mut body)?;
        }
    }
    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line_headers_query_and_body() {
        let raw =
            b"POST /mcp?thread=nova&since=3 HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\n\r\nbody";
        let request = read_request(&mut &raw[..]).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.query_value("thread"), Some("nova"));
        assert_eq!(request.query_value("since"), Some("3"));
        assert_eq!(request.header("CONTENT-LENGTH"), Some("4"));
        assert_eq!(request.body, b"body");
    }

    #[test]
    fn refuses_oversized_or_ambiguous_requests() {
        let huge = format!(
            "GET / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        assert_eq!(
            read_request(&mut huge.as_bytes()),
            Err(HttpError::HeadersTooLarge)
        );
        let body = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(
            read_request(&mut body.as_bytes()),
            Err(HttpError::BodyTooLarge)
        );
        assert_eq!(
            read_request(&mut &b"POST / HTTP/1.1\r\n\r\n"[..]),
            Err(HttpError::LengthRequired)
        );
        assert_eq!(
            read_request(&mut &b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"[..]),
            Err(HttpError::LengthRequired)
        );
        assert!(read_request(&mut &b"GET /x?a=%2e HTTP/1.1\r\n\r\n"[..]).is_err());
        assert!(read_request(&mut &b"GET http://x/ HTTP/1.1\r\n\r\n"[..]).is_err());
        assert_eq!(read_request(&mut &b""[..]), Err(HttpError::Closed));
    }

    #[test]
    fn writes_and_reads_responses() {
        let mut wire = Vec::new();
        write_response(
            &mut wire,
            401,
            "application/json",
            &[("WWW-Authenticate", "Bearer")],
            b"{}",
        )
        .unwrap();
        let response = read_response(&mut &wire[..]).unwrap();
        assert_eq!(response.status, 401);
        assert_eq!(response.body, b"{}");
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "WWW-Authenticate" && value == "Bearer"));
    }
}
