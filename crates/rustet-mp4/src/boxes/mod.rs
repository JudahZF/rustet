//! The box tree: one [`Mp4Box`] enum, header handling, decode dispatch and encoding.

mod containers;
mod leaf;

pub use containers::{Container, Moof, Moov, SampleEntry, Stsd, Traf, Trak, is_encryption_group};
pub use leaf::{
    Frma, Hdlr, Mdat, Mdhd, Mfhd, Mvhd, RawBox, Saio, Sbgp, Schm, Senc, SencSamples, Sgpd,
    SubSample, Tenc, Tfdt, Tfhd, Tkhd, Trex, Trun, TrunSample,
};

use crate::{Error, FourCC, Result};

/// A decoded box header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: FourCC,
    /// Total box size, header included.
    pub size: u64,
    /// 8, or 16 for a 64-bit `largesize` header.
    pub header_len: u8,
}

impl BoxHeader {
    /// Parses the header at the start of `buf`. Returns `Ok(None)` when `buf` is too
    /// short to hold the whole header yet.
    ///
    /// Size 0 ("to end of file") is rejected, as mp4ff does.
    pub fn parse(buf: &[u8]) -> Result<Option<Self>> {
        let Some(head) = buf.get(..8) else {
            return Ok(None);
        };
        let kind = FourCC([head[4], head[5], head[6], head[7]]);
        let (size, header_len) = match be_u32(head, 0) {
            1 => match buf.get(8..16) {
                Some(large) => (be_u64(large, 0), 16),
                None => return Ok(None),
            },
            0 => {
                return Err(Error::InvalidBox {
                    kind,
                    reason: "size 0 (to end of file) not supported",
                });
            }
            n => (u64::from(n), 8),
        };
        if size < u64::from(header_len) {
            return Err(Error::InvalidBox {
                kind,
                reason: "size smaller than its header",
            });
        }
        Ok(Some(Self {
            kind,
            size,
            header_len,
        }))
    }

    pub fn body_len(&self) -> u64 {
        self.size - u64::from(self.header_len)
    }
}

/// Where a box sits: sample entries are only recognised as direct children of `stsd`
/// (an `alac` box is a sample entry there, but a plain config box inside it).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ctx {
    Normal,
    SampleEntries,
}

/// One MP4 box.
///
/// Boxes the stream code reads or rewrites are typed. Every other box is kept as
/// [`RawBox`] and re-encoded byte for byte, so a decode → encode round trip is lossless.
#[derive(Debug, Clone, PartialEq)]
pub enum Mp4Box {
    Moov(Moov),
    Trak(Trak),
    Moof(Moof),
    Traf(Traf),
    /// A plain container: `mdia`, `minf`, `stbl`, `mvex`, `sinf`, `schi`.
    Container(Container),
    Stsd(Stsd),
    /// An `enca` / `encv` entry of `stsd`, renamed to its clear format by decryption.
    SampleEntry(SampleEntry),
    Mvhd(Mvhd),
    Tkhd(Tkhd),
    Mdhd(Mdhd),
    Hdlr(Hdlr),
    Trex(Trex),
    Mfhd(Mfhd),
    Tfhd(Tfhd),
    Tfdt(Tfdt),
    Trun(Trun),
    Senc(Senc),
    Saio(Saio),
    Sbgp(Sbgp),
    Sgpd(Sgpd),
    Frma(Frma),
    Schm(Schm),
    Tenc(Tenc),
    Mdat(Mdat),
    Raw(RawBox),
}

impl Mp4Box {
    pub fn kind(&self) -> FourCC {
        match self {
            Self::Moov(_) => FourCC::MOOV,
            Self::Trak(_) => FourCC::TRAK,
            Self::Moof(_) => FourCC::MOOF,
            Self::Traf(_) => FourCC::TRAF,
            Self::Container(b) => b.kind,
            Self::Stsd(_) => FourCC::STSD,
            Self::SampleEntry(b) => b.kind,
            Self::Mvhd(_) => FourCC::MVHD,
            Self::Tkhd(_) => FourCC::TKHD,
            Self::Mdhd(_) => FourCC::MDHD,
            Self::Hdlr(_) => FourCC::HDLR,
            Self::Trex(_) => FourCC::TREX,
            Self::Mfhd(_) => FourCC::MFHD,
            Self::Tfhd(_) => FourCC::TFHD,
            Self::Tfdt(_) => FourCC::TFDT,
            Self::Trun(_) => FourCC::TRUN,
            Self::Senc(_) => FourCC::SENC,
            Self::Saio(_) => FourCC::SAIO,
            Self::Sbgp(_) => FourCC::SBGP,
            Self::Sgpd(_) => FourCC::SGPD,
            Self::Frma(_) => FourCC::FRMA,
            Self::Schm(_) => FourCC::SCHM,
            Self::Tenc(_) => FourCC::TENC,
            Self::Mdat(_) => FourCC::MDAT,
            Self::Raw(b) => b.kind,
        }
    }

    /// Encoded size in bytes, header included.
    pub fn size(&self) -> u64 {
        let body = self.body_size();
        body + u64::from(self.header_len(body))
    }

    fn header_len(&self, body: u64) -> u8 {
        let large = matches!(self, Self::Mdat(m) if m.large);
        if large || body + 8 > u64::from(u32::MAX) {
            16
        } else {
            8
        }
    }

    fn body_size(&self) -> u64 {
        match self {
            Self::Moov(b) => children_size(&b.children),
            Self::Trak(b) => children_size(&b.children),
            Self::Moof(b) => children_size(&b.children),
            Self::Traf(b) => children_size(&b.children),
            Self::Container(b) => children_size(&b.children),
            Self::Stsd(b) => 8 + children_size(&b.entries),
            Self::SampleEntry(b) => b.body_size(),
            Self::Mvhd(b) => b.payload.len() as u64,
            Self::Tkhd(b) => b.payload.len() as u64,
            Self::Mdhd(b) => b.payload.len() as u64,
            Self::Hdlr(b) => b.payload.len() as u64,
            Self::Trex(_) => 24,
            Self::Mfhd(_) => 8,
            Self::Tfhd(b) => b.body_size(),
            Self::Tfdt(b) => {
                if b.version == 1 {
                    12
                } else {
                    8
                }
            }
            Self::Trun(b) => b.body_size(),
            Self::Senc(b) => 8 + b.raw.len() as u64,
            Self::Saio(b) => b.payload.len() as u64,
            Self::Sbgp(b) => b.payload.len() as u64,
            Self::Sgpd(b) => b.payload.len() as u64,
            Self::Frma(_) => 4,
            Self::Schm(b) => b.payload.len() as u64,
            Self::Tenc(b) => b.payload.len() as u64,
            Self::Mdat(b) => b.data.len() as u64,
            Self::Raw(b) => b.payload.len() as u64,
        }
    }

    /// Appends the encoded box to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<()> {
        let body = self.body_size();
        let size = body + u64::from(self.header_len(body));
        if self.header_len(body) == 16 {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(&self.kind().0);
            out.extend_from_slice(&size.to_be_bytes());
        } else {
            out.extend_from_slice(&(size as u32).to_be_bytes());
            out.extend_from_slice(&self.kind().0);
        }
        match self {
            Self::Moov(b) => encode_children(&b.children, out)?,
            Self::Trak(b) => encode_children(&b.children, out)?,
            Self::Moof(b) => encode_children(&b.children, out)?,
            Self::Traf(b) => encode_children(&b.children, out)?,
            Self::Container(b) => encode_children(&b.children, out)?,
            Self::Stsd(b) => {
                out.extend_from_slice(&b.version_flags.to_be_bytes());
                out.extend_from_slice(&(b.entries.len() as u32).to_be_bytes());
                encode_children(&b.entries, out)?;
            }
            Self::SampleEntry(b) => {
                out.extend_from_slice(&b.header);
                encode_children(&b.children, out)?;
                out.extend_from_slice(&b.trailing);
            }
            Self::Mvhd(b) => out.extend_from_slice(&b.payload),
            Self::Tkhd(b) => out.extend_from_slice(&b.payload),
            Self::Mdhd(b) => out.extend_from_slice(&b.payload),
            Self::Hdlr(b) => out.extend_from_slice(&b.payload),
            Self::Trex(b) => b.encode_body(out),
            Self::Mfhd(b) => {
                out.extend_from_slice(&b.version_flags.to_be_bytes());
                out.extend_from_slice(&b.sequence_number.to_be_bytes());
            }
            Self::Tfhd(b) => b.encode_body(out),
            Self::Tfdt(b) => b.encode_body(out),
            Self::Trun(b) => b.encode_body(out)?,
            Self::Senc(b) => {
                out.extend_from_slice(&b.version_flags.to_be_bytes());
                out.extend_from_slice(&b.sample_count.to_be_bytes());
                out.extend_from_slice(&b.raw);
            }
            Self::Saio(b) => out.extend_from_slice(&b.payload),
            Self::Sbgp(b) => out.extend_from_slice(&b.payload),
            Self::Sgpd(b) => out.extend_from_slice(&b.payload),
            Self::Frma(b) => out.extend_from_slice(&b.data_format.0),
            Self::Schm(b) => out.extend_from_slice(&b.payload),
            Self::Tenc(b) => out.extend_from_slice(&b.payload),
            Self::Mdat(b) => out.extend_from_slice(&b.data),
            Self::Raw(b) => out.extend_from_slice(&b.payload),
        }
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(self.size() as usize);
        self.encode_into(&mut out)?;
        Ok(out)
    }

    /// Decodes one complete box from the start of `buf`, which sits at absolute stream
    /// position `pos`. Returns the box and the number of bytes it used.
    pub fn decode(buf: &[u8], pos: u64) -> Result<(Self, usize)> {
        let header = BoxHeader::parse(buf)?.ok_or(Error::Truncated)?;
        let end = usize::try_from(header.size).map_err(|_| Error::Truncated)?;
        let body = buf
            .get(usize::from(header.header_len)..end)
            .ok_or(Error::Truncated)?;
        Ok((
            Self::decode_body(header, body.to_vec(), pos, Ctx::Normal)?,
            end,
        ))
    }

    /// Builds a box from its header and body bytes.
    pub(crate) fn decode_body(
        header: BoxHeader,
        body: Vec<u8>,
        pos: u64,
        ctx: Ctx,
    ) -> Result<Self> {
        let kind = header.kind;
        let child_pos = pos + u64::from(header.header_len);
        let children = |ctx| decode_children(kind, &body, child_pos, ctx);
        Ok(match (ctx, kind) {
            (Ctx::SampleEntries, FourCC::ENCA) => Self::SampleEntry(SampleEntry::decode(
                kind,
                SampleEntry::AUDIO_HEADER,
                &body,
                child_pos,
            )?),
            (Ctx::SampleEntries, FourCC::ENCV) => Self::SampleEntry(SampleEntry::decode(
                kind,
                SampleEntry::VISUAL_HEADER,
                &body,
                child_pos,
            )?),
            (Ctx::SampleEntries, _) => Self::Raw(RawBox {
                kind,
                payload: body,
            }),
            (_, FourCC::MOOV) => Self::Moov(Moov {
                children: children(Ctx::Normal)?,
            }),
            (_, FourCC::TRAK) => Self::Trak(Trak {
                children: children(Ctx::Normal)?,
            }),
            (_, FourCC::MOOF) => Self::Moof(Moof {
                pos,
                children: children(Ctx::Normal)?,
            }),
            (_, FourCC::TRAF) => Self::Traf(Traf {
                children: children(Ctx::Normal)?,
            }),
            (
                _,
                FourCC::MDIA
                | FourCC::MINF
                | FourCC::STBL
                | FourCC::MVEX
                | FourCC::SINF
                | FourCC::SCHI,
            ) => Self::Container(Container {
                kind,
                children: children(Ctx::Normal)?,
            }),
            (_, FourCC::STSD) => Self::Stsd(Stsd::decode(&body, child_pos)?),
            (_, FourCC::MVHD) => Self::Mvhd(Mvhd::new(body)?),
            (_, FourCC::TKHD) => Self::Tkhd(Tkhd::new(body)?),
            (_, FourCC::MDHD) => Self::Mdhd(Mdhd::new(body)?),
            (_, FourCC::HDLR) => Self::Hdlr(Hdlr::new(body)?),
            (_, FourCC::TREX) => Self::Trex(Trex::decode(&body)?),
            (_, FourCC::MFHD) => Self::Mfhd(Mfhd::decode(&body)?),
            (_, FourCC::TFHD) => Self::Tfhd(Tfhd::decode(&body)?),
            (_, FourCC::TFDT) => Self::Tfdt(Tfdt::decode(&body)?),
            (_, FourCC::TRUN) => Self::Trun(Trun::decode(&body)?),
            (_, FourCC::SENC) => Self::Senc(Senc::decode(body, pos)?),
            (_, FourCC::SAIO) => Self::Saio(Saio::new(body)?),
            (_, FourCC::SBGP) => Self::Sbgp(Sbgp::new(body)?),
            (_, FourCC::SGPD) => Self::Sgpd(Sgpd::new(body)?),
            (_, FourCC::FRMA) => Self::Frma(Frma::decode(&body)?),
            (_, FourCC::SCHM) => Self::Schm(Schm::new(body)?),
            (_, FourCC::TENC) => Self::Tenc(Tenc::new(body)?),
            (_, FourCC::MDAT) => Self::Mdat(Mdat {
                pos,
                data: body,
                large: header.header_len == 16,
            }),
            _ => Self::Raw(RawBox {
                kind,
                payload: body,
            }),
        })
    }
}

fn children_size(children: &[Mp4Box]) -> u64 {
    children.iter().map(Mp4Box::size).sum()
}

fn encode_children(children: &[Mp4Box], out: &mut Vec<u8>) -> Result<()> {
    children.iter().try_for_each(|c| c.encode_into(out))
}

/// Decodes the boxes that fill `data` exactly; `pos` is the absolute position of `data`.
pub(crate) fn decode_children(
    parent: FourCC,
    data: &[u8],
    pos: u64,
    ctx: Ctx,
) -> Result<Vec<Mp4Box>> {
    let mut children = Vec::new();
    let mut off = 0;
    while off < data.len() {
        let rest = &data[off..];
        let header = BoxHeader::parse(rest)?.ok_or(Error::InvalidBox {
            kind: parent,
            reason: "truncated child box header",
        })?;
        let end = usize::try_from(header.size)
            .ok()
            .filter(|&end| end <= rest.len())
            .ok_or(Error::InvalidBox {
                kind: parent,
                reason: "child box exceeds its parent",
            })?;
        let body = rest[usize::from(header.header_len)..end].to_vec();
        children.push(Mp4Box::decode_body(header, body, pos + off as u64, ctx)?);
        off += end;
    }
    Ok(children)
}

/// Typed access to one [`Mp4Box`] variant, used by the [`Children`] finders.
pub trait BoxType: Sized {
    fn cast(b: &Mp4Box) -> Option<&Self>;
    fn cast_mut(b: &mut Mp4Box) -> Option<&mut Self>;
}

macro_rules! box_types {
    ($($variant:ident),* $(,)?) => {$(
        impl BoxType for $variant {
            fn cast(b: &Mp4Box) -> Option<&Self> {
                match b { Mp4Box::$variant(x) => Some(x), _ => None }
            }
            fn cast_mut(b: &mut Mp4Box) -> Option<&mut Self> {
                match b { Mp4Box::$variant(x) => Some(x), _ => None }
            }
        }
        impl From<$variant> for Mp4Box {
            fn from(b: $variant) -> Self { Mp4Box::$variant(b) }
        }
    )*};
}

box_types!(
    Moov,
    Trak,
    Moof,
    Traf,
    Stsd,
    SampleEntry,
    Mvhd,
    Tkhd,
    Mdhd,
    Hdlr,
    Trex,
    Mfhd,
    Tfhd,
    Tfdt,
    Trun,
    Senc,
    Saio,
    Sbgp,
    Sgpd,
    Frma,
    Schm,
    Tenc,
    Mdat,
);

impl From<Container> for Mp4Box {
    fn from(b: Container) -> Self {
        Mp4Box::Container(b)
    }
}

impl From<RawBox> for Mp4Box {
    fn from(b: RawBox) -> Self {
        Mp4Box::Raw(b)
    }
}

/// Anything with an ordered list of child boxes. The finders return the first match,
/// like mp4ff's typed child pointers.
pub trait Children {
    fn children(&self) -> &[Mp4Box];
    fn children_mut(&mut self) -> &mut Vec<Mp4Box>;

    fn first<T: BoxType>(&self) -> Option<&T> {
        self.children().iter().find_map(T::cast)
    }

    fn first_mut<T: BoxType>(&mut self) -> Option<&mut T> {
        self.children_mut().iter_mut().find_map(T::cast_mut)
    }

    fn all<'a, T: BoxType + 'a>(&'a self) -> impl Iterator<Item = &'a T> {
        self.children().iter().filter_map(T::cast)
    }

    fn all_mut<'a, T: BoxType + 'a>(&'a mut self) -> impl Iterator<Item = &'a mut T> {
        self.children_mut().iter_mut().filter_map(T::cast_mut)
    }

    fn child(&self, kind: FourCC) -> Option<&Mp4Box> {
        self.children().iter().find(|c| c.kind() == kind)
    }

    /// The first plain [`Container`] child of `kind` (`mdia`, `minf`, `stbl`, `mvex`, ...).
    fn container(&self, kind: FourCC) -> Option<&Container> {
        self.children().iter().find_map(|c| match c {
            Mp4Box::Container(b) if b.kind == kind => Some(b),
            _ => None,
        })
    }

    fn container_mut(&mut self, kind: FourCC) -> Option<&mut Container> {
        self.children_mut().iter_mut().find_map(|c| match c {
            Mp4Box::Container(b) if b.kind == kind => Some(b),
            _ => None,
        })
    }

    /// Appends a child (mp4ff `AddChild`).
    fn push(&mut self, child: impl Into<Mp4Box>) {
        self.children_mut().push(child.into());
    }

    /// Removes every child of `kind` and returns the removed boxes in order.
    fn remove_kind(&mut self, kind: FourCC) -> Vec<Mp4Box> {
        let (removed, kept) = std::mem::take(self.children_mut())
            .into_iter()
            .partition(|c| c.kind() == kind);
        *self.children_mut() = kept;
        removed
    }

    /// Encoded size of the children (a container's body size).
    fn children_size(&self) -> u64 {
        children_size(self.children())
    }
}

// Big-endian helpers. Callers check bounds first.
pub(crate) fn be_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn be_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_be_bytes(a)
}

/// A bounds-checked big-endian reader over a box body.
pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    kind: FourCC,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8], kind: FourCC) -> Self {
        Self { data, pos: 0, kind }
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len());
        let end = end.ok_or(Error::InvalidBox {
            kind: self.kind,
            reason: "body too short",
        })?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(be_u32(self.bytes(4)?, 0))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(be_u64(self.bytes(8)?, 0))
    }

    pub(crate) fn fourcc(&mut self) -> Result<FourCC> {
        let b = self.bytes(4)?;
        Ok(FourCC([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
}
