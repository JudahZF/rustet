use std::fmt;

/// A four-character code: box types, handler types, scheme types, sample entry formats.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    pub const FTYP: Self = Self(*b"ftyp");
    pub const MOOV: Self = Self(*b"moov");
    pub const MVHD: Self = Self(*b"mvhd");
    pub const TRAK: Self = Self(*b"trak");
    pub const TKHD: Self = Self(*b"tkhd");
    pub const MDIA: Self = Self(*b"mdia");
    pub const MDHD: Self = Self(*b"mdhd");
    pub const HDLR: Self = Self(*b"hdlr");
    pub const MINF: Self = Self(*b"minf");
    pub const STBL: Self = Self(*b"stbl");
    pub const STSD: Self = Self(*b"stsd");
    pub const MVEX: Self = Self(*b"mvex");
    pub const TREX: Self = Self(*b"trex");
    pub const PSSH: Self = Self(*b"pssh");
    pub const SINF: Self = Self(*b"sinf");
    pub const FRMA: Self = Self(*b"frma");
    pub const ALAC: Self = Self(*b"alac");
    pub const SCHM: Self = Self(*b"schm");
    pub const SCHI: Self = Self(*b"schi");
    pub const TENC: Self = Self(*b"tenc");
    pub const ENCA: Self = Self(*b"enca");
    pub const ENCV: Self = Self(*b"encv");
    pub const MOOF: Self = Self(*b"moof");
    pub const MFHD: Self = Self(*b"mfhd");
    pub const TRAF: Self = Self(*b"traf");
    pub const TFHD: Self = Self(*b"tfhd");
    pub const TFDT: Self = Self(*b"tfdt");
    pub const TRUN: Self = Self(*b"trun");
    pub const SENC: Self = Self(*b"senc");
    pub const SAIZ: Self = Self(*b"saiz");
    pub const SAIO: Self = Self(*b"saio");
    pub const SBGP: Self = Self(*b"sbgp");
    pub const SGPD: Self = Self(*b"sgpd");
    pub const MDAT: Self = Self(*b"mdat");
    pub const EMSG: Self = Self(*b"emsg");
    pub const PRFT: Self = Self(*b"prft");
    pub const STYP: Self = Self(*b"styp");
    pub const SIDX: Self = Self(*b"sidx");
    pub const SSIX: Self = Self(*b"ssix");
    /// Scheme types.
    pub const CENC: Self = Self(*b"cenc");
    pub const CBCS: Self = Self(*b"cbcs");
    /// Sample group types used for encryption (`sbgp` / `sgpd` grouping types).
    pub const SEIG: Self = Self(*b"seig");
    pub const SEAM: Self = Self(*b"seam");
    /// Handler types.
    pub const SOUN: Self = Self(*b"soun");
    pub const VIDE: Self = Self(*b"vide");
}

impl fmt::Display for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            let c = if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            };
            write!(f, "{c}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

impl PartialEq<&[u8; 4]> for FourCC {
    fn eq(&self, other: &&[u8; 4]) -> bool {
        &self.0 == *other
    }
}
