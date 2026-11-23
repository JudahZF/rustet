use super::{
    BoxHeader, Children, Ctx, Frma, Hdlr, Mdhd, Mfhd, Mp4Box, Mvhd, RawBox, Saio, Sbgp, Senc, Sgpd,
    Tfdt, Tfhd, Tkhd, Trex, Trun, decode_children,
};
use crate::{Error, FourCC, Result};

macro_rules! impl_children {
    ($ty:ty, $field:ident) => {
        impl Children for $ty {
            fn children(&self) -> &[Mp4Box] {
                &self.$field
            }
            fn children_mut(&mut self) -> &mut Vec<Mp4Box> {
                &mut self.$field
            }
        }
    };
}

/// A container without box-specific logic: `mdia`, `minf`, `stbl`, `mvex`, `sinf`, `schi`.
#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub kind: FourCC,
    pub children: Vec<Mp4Box>,
}
impl_children!(Container, children);

impl Container {
    pub fn new(kind: FourCC, children: Vec<Mp4Box>) -> Self {
        Self { kind, children }
    }
}

/// `moov`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Moov {
    pub children: Vec<Mp4Box>,
}
impl_children!(Moov, children);

impl Moov {
    pub fn mvhd(&self) -> Option<&Mvhd> {
        self.first()
    }

    pub fn mvhd_mut(&mut self) -> Option<&mut Mvhd> {
        self.first_mut()
    }

    pub fn traks(&self) -> impl Iterator<Item = &Trak> {
        self.all()
    }

    pub fn traks_mut(&mut self) -> impl Iterator<Item = &mut Trak> {
        self.all_mut()
    }

    pub fn mvex(&self) -> Option<&Container> {
        self.container(FourCC::MVEX)
    }

    /// The `trex` boxes of `mvex` (mp4ff `Moov.Mvex.Trexs`).
    pub fn trexs(&self) -> impl Iterator<Item = &Trex> {
        self.mvex().into_iter().flat_map(|m| m.all::<Trex>())
    }

    /// Removes and returns all `pssh` children.
    pub fn remove_psshs(&mut self) -> Vec<RawBox> {
        raw_boxes(self.remove_kind(FourCC::PSSH))
    }
}

/// `trak`. The accessors follow the standard path (`mdia/minf/stbl/...`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Trak {
    pub children: Vec<Mp4Box>,
}
impl_children!(Trak, children);

impl Trak {
    pub fn tkhd(&self) -> Option<&Tkhd> {
        self.first()
    }

    pub fn tkhd_mut(&mut self) -> Option<&mut Tkhd> {
        self.first_mut()
    }

    pub fn track_id(&self) -> Option<u32> {
        self.tkhd().map(Tkhd::track_id)
    }

    pub fn mdia(&self) -> Option<&Container> {
        self.container(FourCC::MDIA)
    }

    pub fn mdia_mut(&mut self) -> Option<&mut Container> {
        self.container_mut(FourCC::MDIA)
    }

    pub fn mdhd(&self) -> Option<&Mdhd> {
        self.mdia()?.first()
    }

    pub fn mdhd_mut(&mut self) -> Option<&mut Mdhd> {
        self.mdia_mut()?.first_mut()
    }

    pub fn hdlr(&self) -> Option<&Hdlr> {
        self.mdia()?.first()
    }

    /// `hdlr` handler type: `soun`, `vide`, ...
    pub fn handler_type(&self) -> Option<FourCC> {
        self.hdlr().map(Hdlr::handler_type)
    }

    pub fn stbl(&self) -> Option<&Container> {
        self.mdia()?
            .container(FourCC::MINF)?
            .container(FourCC::STBL)
    }

    pub fn stbl_mut(&mut self) -> Option<&mut Container> {
        self.mdia_mut()?
            .container_mut(FourCC::MINF)?
            .container_mut(FourCC::STBL)
    }

    pub fn stsd(&self) -> Option<&Stsd> {
        self.stbl()?.first()
    }

    pub fn stsd_mut(&mut self) -> Option<&mut Stsd> {
        self.stbl_mut()?.first_mut()
    }
}

/// `moof`. `pos` is the absolute stream position it was decoded at; sample data is
/// located relative to it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Moof {
    pub pos: u64,
    pub children: Vec<Mp4Box>,
}
impl_children!(Moof, children);

impl Moof {
    pub fn mfhd(&self) -> Option<&Mfhd> {
        self.first()
    }

    pub fn mfhd_mut(&mut self) -> Option<&mut Mfhd> {
        self.first_mut()
    }

    /// The first traf (mp4ff `Moof.Traf`).
    pub fn traf(&self) -> Option<&Traf> {
        self.first()
    }

    pub fn traf_mut(&mut self) -> Option<&mut Traf> {
        self.first_mut()
    }

    pub fn trafs(&self) -> impl Iterator<Item = &Traf> {
        self.all()
    }

    pub fn trafs_mut(&mut self) -> impl Iterator<Item = &mut Traf> {
        self.all_mut()
    }

    /// Removes all `pssh` children and returns the number of bytes removed.
    pub fn remove_psshs(&mut self) -> u64 {
        self.remove_kind(FourCC::PSSH)
            .iter()
            .map(Mp4Box::size)
            .sum()
    }

    /// Encoded size, header included (mp4ff `Moof.Size()`).
    pub fn size(&self) -> u64 {
        self.children_size() + 8
    }
}

/// `traf`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Traf {
    pub children: Vec<Mp4Box>,
}
impl_children!(Traf, children);

impl Traf {
    pub fn tfhd(&self) -> Option<&Tfhd> {
        self.first()
    }

    pub fn tfhd_mut(&mut self) -> Option<&mut Tfhd> {
        self.first_mut()
    }

    pub fn tfdt(&self) -> Option<&Tfdt> {
        self.first()
    }

    pub fn tfdt_mut(&mut self) -> Option<&mut Tfdt> {
        self.first_mut()
    }

    /// The first trun (mp4ff `Traf.Trun`).
    pub fn trun(&self) -> Option<&Trun> {
        self.first()
    }

    pub fn trun_mut(&mut self) -> Option<&mut Trun> {
        self.first_mut()
    }

    pub fn truns(&self) -> impl Iterator<Item = &Trun> {
        self.all()
    }

    pub fn truns_mut(&mut self) -> impl Iterator<Item = &mut Trun> {
        self.all_mut()
    }

    pub fn senc(&self) -> Option<&Senc> {
        self.first()
    }

    pub fn saio(&self) -> Option<&Saio> {
        self.first()
    }

    pub fn sbgp(&self) -> Option<&Sbgp> {
        self.first()
    }

    pub fn sgpd(&self) -> Option<&Sgpd> {
        self.first()
    }

    /// Removes `senc`, `saiz`, `saio` and the `seig` / `seam` sample groups. Returns the
    /// number of bytes removed (mp4ff `RemoveEncryptionBoxes`).
    pub fn remove_encryption_boxes(&mut self) -> u64 {
        let mut removed = 0;
        self.children.retain(|c| {
            let encryption = match c {
                Mp4Box::Senc(_) | Mp4Box::Saio(_) => true,
                Mp4Box::Raw(r) => r.kind == FourCC::SAIZ,
                Mp4Box::Sbgp(b) => is_encryption_group(b.grouping_type),
                Mp4Box::Sgpd(b) => is_encryption_group(b.grouping_type),
                _ => false,
            };
            if encryption {
                removed += c.size();
            }
            !encryption
        });
        removed
    }
}

/// Whether a sample group (`sbgp` / `sgpd` grouping type) carries encryption
/// signalling (`seig` key / IV info or `seam`).
pub fn is_encryption_group(grouping_type: FourCC) -> bool {
    grouping_type == FourCC::SEIG || grouping_type == FourCC::SEAM
}

/// `stsd`. The entry count is not stored: it is always `entries.len()`.
#[derive(Debug, Clone, PartialEq)]
pub struct Stsd {
    pub version_flags: u32,
    pub entries: Vec<Mp4Box>,
}
impl_children!(Stsd, entries);

impl Stsd {
    pub(super) fn decode(body: &[u8], pos: u64) -> Result<Self> {
        let kind = FourCC::STSD;
        let head = body.get(..8).ok_or(Error::InvalidBox {
            kind,
            reason: "body too short",
        })?;
        let version_flags = super::be_u32(head, 0);
        let count = super::be_u32(head, 4);
        let entries = decode_children(kind, &body[8..], pos + 8, Ctx::SampleEntries)?;
        if entries.len() as u64 != u64::from(count) {
            return Err(Error::InvalidBox {
                kind,
                reason: "sample count mismatch",
            });
        }
        Ok(Self {
            version_flags,
            entries,
        })
    }
}

/// An encrypted sample entry (`enca` / `encv`): the fixed header bytes, the child boxes
/// (`esds`, `avcC`, `sinf`, ...) and any sub-header trailing bytes, all kept verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleEntry {
    /// `enca` / `encv`, or the clear format once [`remove_encryption`](Self::remove_encryption) ran.
    pub kind: FourCC,
    pub header: Vec<u8>,
    pub children: Vec<Mp4Box>,
    pub trailing: Vec<u8>,
}
impl_children!(SampleEntry, children);

impl SampleEntry {
    /// Bytes before the children of an audio sample entry.
    pub(super) const AUDIO_HEADER: usize = 28;
    /// Bytes before the children of a visual sample entry.
    pub(super) const VISUAL_HEADER: usize = 78;

    pub(super) fn decode(kind: FourCC, header_len: usize, body: &[u8], pos: u64) -> Result<Self> {
        let header = body.get(..header_len).ok_or(Error::InvalidBox {
            kind,
            reason: "body too short",
        })?;
        let rest = &body[header_len..];
        // Fewer than 8 bytes cannot be a box: keep them, as mp4ff does for visual entries.
        let mut boxes_len = 0;
        while rest.len() - boxes_len >= 8 {
            let at = &rest[boxes_len..];
            let size = BoxHeader::parse(at)?
                .and_then(|h| usize::try_from(h.size).ok())
                .filter(|&size| size <= at.len())
                .ok_or(Error::InvalidBox {
                    kind,
                    reason: "child box exceeds its parent",
                })?;
            boxes_len += size;
        }
        let children = decode_children(
            kind,
            &rest[..boxes_len],
            pos + header_len as u64,
            Ctx::Normal,
        )?;
        Ok(Self {
            kind,
            header: header.to_vec(),
            children,
            trailing: rest[boxes_len..].to_vec(),
        })
    }

    pub(super) fn body_size(&self) -> u64 {
        (self.header.len() + self.trailing.len()) as u64 + self.children_size()
    }

    pub fn sinf(&self) -> Option<&Container> {
        self.container(FourCC::SINF)
    }

    /// Removes the `sinf` box and renames the entry to the clear format from `frma`
    /// (mp4ff `RemoveEncryption`). Returns the removed `sinf`.
    pub fn remove_encryption(&mut self) -> Result<Container> {
        if self.kind != FourCC::ENCA && self.kind != FourCC::ENCV {
            return Err(Error::NotEncrypted(self.kind));
        }
        let (at, format) = self
            .children
            .iter()
            .enumerate()
            .find_map(|(i, c)| match c {
                Mp4Box::Container(b) if b.kind == FourCC::SINF => {
                    Some((i, b.first::<Frma>().map(|f| f.data_format)))
                }
                _ => None,
            })
            .ok_or(Error::MissingBox("sinf"))?;
        let format = format.ok_or(Error::MissingBox("frma"))?;
        let Mp4Box::Container(sinf) = self.children.remove(at) else {
            unreachable!("index found above")
        };
        self.kind = format;
        Ok(sinf)
    }
}

fn raw_boxes(boxes: Vec<Mp4Box>) -> Vec<RawBox> {
    boxes
        .into_iter()
        .filter_map(|b| match b {
            Mp4Box::Raw(r) => Some(r),
            _ => None,
        })
        .collect()
}
