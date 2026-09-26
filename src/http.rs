//! A minimal HTTP/1.1 client: one GET per connection, whole reply in memory.
//!
//! This is all the custard cream server API needs: small JSON replies and
//! JPEGs of a few hundred kilobytes. Replies may use `Content-Length` or
//! chunked transfer encoding. There is no TLS, so only http:// works.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, with_timeout};
use embedded_io_async::Write;

/// Gives up if the server stops sending for this long.
const TIMEOUT: Duration = Duration::from_secs(20);

/// How often to check that an idle connection is still there. smoltcp's
/// timeout only covers waiting for our own data to be acknowledged; with
/// keep-alive on, it also covers a server that goes quiet while we wait to
/// receive.
const KEEP_ALIVE: Duration = Duration::from_secs(5);

/// Gives up on a whole request (look-up, connect and reply) after this long,
/// whatever the socket is doing.
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// Room for the status line and headers on top of the body limit.
const MAX_HEADER_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub enum Error {
    /// The host name couldn't be looked up.
    Dns,
    /// The server didn't accept the connection.
    Connect,
    /// The connection failed or timed out part way through.
    Network,
    /// The whole request took longer than [`REQUEST_DEADLINE`].
    Timeout,
    /// The server answered with something other than 200 OK.
    Status(u16),
    /// The reply wasn't valid HTTP, or the body wasn't what was expected.
    BadReply,
    /// The reply was bigger than the caller allowed.
    TooBig,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Dns => write!(f, "can't find the server"),
            Error::Connect => write!(f, "can't connect to the server"),
            Error::Network => write!(f, "connection lost"),
            Error::Timeout => write!(f, "the server took too long"),
            Error::Status(code) => write!(f, "server error {code}"),
            Error::BadReply => write!(f, "unexpected reply from the server"),
            Error::TooBig => write!(f, "reply too big"),
        }
    }
}

/// Fetches `http://host:port{path}` and returns the body of a 200 reply.
/// `path` must already be percent-encoded.
pub async fn get(
    stack: Stack<'_>,
    host: &str,
    port: u16,
    path: &str,
    max_body: usize,
) -> Result<Vec<u8>, Error> {
    with_timeout(REQUEST_DEADLINE, get_inner(stack, host, port, path, max_body))
        .await
        .unwrap_or(Err(Error::Timeout))
}

async fn get_inner(
    stack: Stack<'_>,
    host: &str,
    port: u16,
    path: &str,
    max_body: usize,
) -> Result<Vec<u8>, Error> {
    let address = *stack
        .dns_query(host, DnsQueryType::A)
        .await
        .map_err(|_| Error::Dns)?
        .first()
        .ok_or(Error::Dns)?;

    // The receive buffer is big enough to come from PSRAM, leaving internal
    // RAM for the Wi-Fi driver.
    let mut rx = vec![0u8; 16 * 1024];
    let mut tx = vec![0u8; 1024];
    let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
    socket.set_timeout(Some(TIMEOUT));
    socket.set_keep_alive(Some(KEEP_ALIVE));
    socket
        .connect((address, port))
        .await
        .map_err(|_| Error::Connect)?;

    let host_header: String = if port == 80 {
        host.into()
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: custard-cream-display/{}\r\n\
         Accept-Encoding: identity\r\n\
         Connection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    socket
        .write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Network)?;

    // "Connection: close": the reply ends when the server closes.
    let limit = max_body + MAX_HEADER_BYTES;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match socket.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if reply.len() + n > limit {
                    socket.abort();
                    return Err(Error::TooBig);
                }
                reply.extend_from_slice(&chunk[..n]);
            }
            Err(_) => {
                socket.abort();
                return Err(Error::Network);
            }
        }
    }
    socket.close();
    let _ = socket.flush().await;

    parse_reply(reply, max_body)
}

/// Splits a complete reply into status, headers and body.
fn parse_reply(mut reply: Vec<u8>, max_body: usize) -> Result<Vec<u8>, Error> {
    let header_end = reply
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(Error::BadReply)?;
    let head = core::str::from_utf8(&reply[..header_end]).map_err(|_| Error::BadReply)?;
    let mut lines = head.split("\r\n");

    // "HTTP/1.1 200 OK"
    let status: u16 = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(Error::BadReply)?;

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse::<usize>().map_err(|_| Error::BadReply)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }

    if status != 200 {
        return Err(Error::Status(status));
    }

    let body = reply.split_off(header_end + 4);
    let body = if chunked {
        dechunk(&body)?
    } else if let Some(length) = content_length {
        if body.len() < length {
            // The connection closed early.
            return Err(Error::Network);
        }
        let mut body = body;
        body.truncate(length);
        body
    } else {
        body
    };
    if body.len() > max_body {
        return Err(Error::TooBig);
    }
    Ok(body)
}

/// Decodes a chunked body: `<hex size>[;ext]\r\n<data>\r\n ... 0\r\n\r\n`.
fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, Error> {
    let mut body = Vec::with_capacity(data.len());
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or(Error::Network)?;
        let size_text = core::str::from_utf8(&data[..line_end]).map_err(|_| Error::BadReply)?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| Error::BadReply)?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(body);
        }
        if data.len() < size + 2 {
            return Err(Error::Network);
        }
        body.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// Percent-encodes a path segment or query value.
pub fn encode(text: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &b in text.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xF) as usize] as char);
        }
    }
}
