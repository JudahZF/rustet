//! Box-by-box readers for streams (mp4ff `DecodeBox(offset, r)`).

use std::io::Read;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::boxes::{BoxHeader, Ctx};
use crate::{Error, Mp4Box, Result};

/// Reads the next box from `r`, which is at absolute stream position `pos`.
///
/// Returns `Ok(None)` at a clean end of input (no byte of a new box), and
/// [`Error::Truncated`] when the input ends inside a box. The body buffer grows with the
/// bytes actually received, so a lying size field cannot force a huge allocation.
pub fn read_box<R: Read>(r: &mut R, pos: u64) -> Result<Option<Mp4Box>> {
    let mut head = [0u8; 16];
    let Some(header) = read_header(&mut head, |buf| read_full(r, buf))? else {
        return Ok(None);
    };
    let mut body = Vec::new();
    r.take(header.body_len()).read_to_end(&mut body)?;
    finish(header, body, pos).map(Some)
}

/// Async form of [`read_box`].
pub async fn read_box_async<R: AsyncRead + Unpin>(r: &mut R, pos: u64) -> Result<Option<Mp4Box>> {
    let mut head = [0u8; 16];
    let n = read_full_async(r, &mut head[..8]).await?;
    if n == 0 {
        return Ok(None);
    }
    if n < 8 {
        return Err(Error::Truncated);
    }
    if head[..4] == [0, 0, 0, 1] && read_full_async(r, &mut head[8..]).await? < 8 {
        return Err(Error::Truncated);
    }
    let header = BoxHeader::parse(&head)?.ok_or(Error::Truncated)?;
    let mut body = Vec::new();
    (&mut *r)
        .take(header.body_len())
        .read_to_end(&mut body)
        .await?;
    finish(header, body, pos).map(Some)
}

/// Reads an 8 or 16 byte header through `read`, which fills a buffer as far as the
/// input allows and returns the count.
fn read_header(
    head: &mut [u8; 16],
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
) -> Result<Option<BoxHeader>> {
    match read(&mut head[..8])? {
        0 => return Ok(None),
        8 => {}
        _ => return Err(Error::Truncated),
    }
    if head[..4] == [0, 0, 0, 1] && read(&mut head[8..])? < 8 {
        return Err(Error::Truncated);
    }
    BoxHeader::parse(head)?.ok_or(Error::Truncated).map(Some)
}

fn finish(header: BoxHeader, body: Vec<u8>, pos: u64) -> Result<Mp4Box> {
    if (body.len() as u64) < header.body_len() {
        return Err(Error::Truncated);
    }
    Mp4Box::decode_body(header, body, pos, Ctx::Normal)
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

async fn read_full_async<R: AsyncRead + Unpin>(
    r: &mut R,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]).await? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
