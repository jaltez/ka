//! Shared Content-Length JSON-RPC framing: the LSP base protocol and
//! the DAP transport are the same wire format (`Content-Length: N\r\n`
//! `\r\n` + N body bytes). Consumers: `lsp.rs`, `dap.rs`. MCP stdio is
//! newline-delimited and deliberately NOT this — its reader is the line
//! codec. No new dependency, same precedent as everything in this
//! crate's protocol layer.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A hostile or buggy Content-Length must not drive an allocation:
/// frames larger than this close the connection instead.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Write one framed body.
pub async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    w: &mut W,
    body: &[u8],
) -> std::io::Result<()> {
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(body).await?;
    w.flush().await
}

/// Read one framed body. `None` = the pipe is done (clean EOF, a framing
/// lie bigger than [`MAX_FRAME`], or a mid-frame read error — in every
/// one of those cases the only sane move is to stop reading).
/// `Some(bytes)` with an empty vec means a zero-length frame; callers
/// that require JSON will fail the parse and keep looping, mirroring
/// the "stray blank lines between messages" tolerance of the header
/// reader.
pub async fn read_frame<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Option<Vec<u8>> {
    let headers = read_headers(reader).await.ok()??;
    // header names are case-insensitive per the LSP base protocol
    let len: usize = headers
        .iter()
        .find_map(|h| {
            let (k, v) = h.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    match len {
        0 => Some(Vec::new()),
        l if l > MAX_FRAME => None,
        l => {
            let mut body = vec![0u8; l];
            reader.read_exact(&mut body).await.ok()?;
            Some(body)
        }
    }
}

/// Read one header block (to the blank line); `None` = EOF.
async fn read_headers<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<String>>> {
    let mut headers: Vec<String> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // read one line terminated by \n (header lines end \r\n)
        let mut line = Vec::new();
        loop {
            match reader.read(&mut byte).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => line.push(byte[0]),
            }
        }
        let line = String::from_utf8_lossy(&line)
            .trim_end_matches('\r')
            .to_string();
        if line.is_empty() {
            if headers.is_empty() {
                continue; // stray blank lines between messages
            }
            return Ok(Some(std::mem::take(&mut headers)));
        }
        headers.push(line);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_tolerate_noise() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"a":1}"#).await.unwrap();
        write_frame(&mut buf, b"second").await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), br#"{"a":1}"#);
        assert_eq!(read_frame(&mut r).await.unwrap(), b"second");
        assert!(read_frame(&mut r).await.is_none(), "EOF is None");
    }

    #[tokio::test]
    async fn zero_length_frames_surface_as_empty() {
        // callers fail the JSON parse on an empty body and keep looping
        // — same net effect as the pre-extraction `continue`
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").await.unwrap();
        write_frame(&mut buf, b"after").await.unwrap();
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), Vec::<u8>::new());
        assert_eq!(read_frame(&mut r).await.unwrap(), b"after");
    }

    #[tokio::test]
    async fn oversize_frames_close_instead_of_allocating() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"tiny").await.unwrap();
        let mut hostile = format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1).into_bytes();
        hostile.extend_from_slice(&buf);
        let mut r = std::io::Cursor::new(hostile);
        assert!(read_frame(&mut r).await.is_none(), "a framing lie closes");
    }

    #[tokio::test]
    async fn content_length_is_case_insensitive() {
        let mut buf = b"content-length: 2\r\n\r\nhi".to_vec();
        let mut r = std::io::Cursor::new(&mut buf);
        assert_eq!(read_frame(&mut r).await.unwrap(), b"hi");
    }
}
