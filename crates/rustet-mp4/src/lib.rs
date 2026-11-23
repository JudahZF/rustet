//! ISO BMFF (MP4 / fragmented MP4) box parsing and rewriting used by the stream decryptors.
//!
//! This crate replaces the Go library `github.com/itouakirai/mp4ff` (a fork of
//! Eyevinn/mp4ff) for the FairPlay CBCS pipeline, the AAC / music-video streams and the
//! ALAC remux. It covers what those callers need, not MP4 in general:
//!
//! - Box-by-box reading from a stream ([`read_box`], [`read_box_async`]) or a slice
//!   ([`Mp4Box::decode`]), with absolute positions recorded on `moof`, `mdat` and `senc`.
//! - A box tree ([`Mp4Box`]) that is typed where the callers read or edit fields and
//!   raw everywhere else. Decode → encode is lossless; for the mp4ff test streams in
//!   `tests/data` the output is byte-identical to mp4ff's.
//! - [`InitSegment`] / [`Fragment`] with mp4ff's encode rules (single-trun data offset).
//! - Common Encryption: [`decrypt_init`], [`decrypt_fragment`] (`cenc` and `cbcs` with a
//!   key) and the pieces for socket-based decryption (senc parsing, sample location,
//!   encryption-box removal).
//!
//! Malformed input returns an [`Error`]; nothing panics on stream data.
//!
//! # Porting table
//!
//! `mp4` is the mp4ff package. Typed child finders come from the [`Children`] trait
//! (`use rustet_mp4::Children;`): `first::<T>()`, `all::<T>()`, `container(kind)`,
//! `push(box)`, `remove_kind(kind)`, and their `_mut` forms.
//!
//! **Reading**
//!
//! | mp4ff | rustet-mp4 |
//! |---|---|
//! | `mp4.DecodeBox(offset, r)`; `err == io.EOF` | [`read_box(&mut r, offset)`](read_box) / [`read_box_async`]; `Ok(None)` at a clean end |
//! | `io.ErrUnexpectedEOF` (also wrapped) | [`Error::Truncated`] |
//! | `mp4.DecodeBox(0, bytes.NewReader(b))` | [`Mp4Box::decode(&b, 0)`](Mp4Box::decode) → `(box, used_len)` |
//! | `box.Type()` / `box.Size()` | [`Mp4Box::kind`] ([`FourCC`], compare with `FourCC::MOOV` or `== b"moov"`) / [`Mp4Box::size`] |
//! | `box.(*mp4.MoofBox)` / `*mp4.MoovBox` | `if let Mp4Box::Moof(moof) = box` / `Mp4Box::Moov` |
//! | `mp4.NewMP4Init()`; `init.AddChild(box)` | `InitSegment::default()`; `init.push(box)` |
//! | `mp4.NewFragment()`; `frag.AddChild(box)` | `Fragment::default()`; `frag.push(box)` |
//! | manual box header parsing (`direct.go`, `mvindex.go`) | [`BoxHeader::parse`] (`Ok(None)` until the whole header is there) |
//!
//! **Init segment**
//!
//! | mp4ff | rustet-mp4 |
//! |---|---|
//! | `init.Moov` (nil check) | [`init.moov()`](InitSegment::moov) / `moov_mut()` → `Option` |
//! | `init.Moov.Mvhd.Timescale` / `.Duration = d` | `moov.mvhd()?.timescale()` / `moov.mvhd_mut()?.set_duration(d)` |
//! | `init.Moov.Traks`, `init.Moov.Trak` | `moov.traks()` / `traks_mut()`, `.next()` for the first |
//! | `trak.Tkhd.TrackID` / `trak.Tkhd.Duration = d` | [`trak.track_id()`](Trak::track_id) / `trak.tkhd_mut()?.set_duration(d)` |
//! | `trak.Mdia.Mdhd.Timescale` / `.Duration = d` | `trak.mdhd()?.timescale()` / `trak.mdhd_mut()?.set_duration(d)` |
//! | `trak.Mdia.Hdlr.HandlerType == "vide"` | `trak.handler_type() == Some(FourCC::VIDE)` |
//! | `trak.Mdia.Minf.Stbl` / `.Stsd` | `trak.stbl()` / `stbl_mut()` / `stsd()` / `stsd_mut()` |
//! | `stsd.Children`, `child.Type()` | [`stsd.entries`](Stsd::entries), `entry.kind()` |
//! | `stsd.Children = children[:1]; stsd.SampleCount = 1` | `stsd.entries.truncate(1)` (the count is always `entries.len()`) |
//! | `stbl.Children` + `*mp4.SbgpBox` / `*mp4.SgpdBox` `.GroupingType` | `stbl.children.retain(...)` with `Mp4Box::Sbgp(b)` / `Mp4Box::Sgpd(b)` `b.grouping_type` ([`is_encryption_group`]) |
//! | `init.Moov.Mvex == nil` | `moov.mvex().is_none()` |
//! | `mvex := mp4.NewMvexBox(); trex := mp4.CreateTrex(id); trex.DefaultSampleDuration = 4096; mvex.AddChild(trex); init.Moov.AddChild(mvex)` | `moov.push(Container::new(FourCC::MVEX, vec![Trex { default_sample_duration: 4096, ..Trex::new(id) }.into()]))` |
//! | `init.Encode(w)` | [`init.to_bytes()`](InitSegment::to_bytes) / `encode_into(&mut buf)` |
//!
//! **Fragments**
//!
//! | mp4ff | rustet-mp4 |
//! |---|---|
//! | `frag.Moof`, `frag.Mdat` (nil checks) | [`frag.moof()`](Fragment::moof) / `moof_mut()`, `frag.mdat()` / `mdat_mut()` |
//! | `frag.Mdat.Data`, `len(...)`, `frag.Mdat.Size()` | `mdat.data`, `mdat.data.len()`, `Mp4Box::size` on the box or `data.len() + header_size()` |
//! | `frag.Moof.Size()` | [`moof.size()`](Moof::size) |
//! | `frag.Moof.Mfhd.SequenceNumber = n` | `moof.mfhd_mut()?.sequence_number = n` |
//! | `frag.Moof.Traf` / `.Trafs` | `moof.traf()` / `traf_mut()` (first), `moof.trafs()` / `trafs_mut()` |
//! | `traf.Tfhd.TrackID = 1` | `traf.tfhd_mut()?.track_id = 1` |
//! | `tfhd.HasSampleDescriptionIndex()`, `.SampleDescriptionIndex = 1` | [`tfhd.has_sample_description_index()`](Tfhd::has_sample_description_index), `tfhd.sample_description_index = 1` |
//! | `tfhd.HasDefaultSampleDuration()`, `.DefaultSampleDuration` | `tfhd.has_default_sample_duration()`, `tfhd.default_sample_duration` |
//! | `traf.Tfdt.BaseMediaDecodeTime()` | `traf.tfdt()?.base_media_decode_time()` |
//! | `traf.Tfdt.SetBaseMediaDecodeTime(t); traf.Tfdt.Version = 1` | `let tfdt = traf.tfdt_mut()?; tfdt.set_base_media_decode_time(t); tfdt.version = 1;` |
//! | `tfdt := mp4.CreateTfdt(t)`; insert into `traf.Children` before `trun` | `Tfdt::new(t)` (then `version = 1`); insert `Mp4Box::Tfdt(tfdt)` into `traf.children` |
//! | `traf.Trun` / `traf.Truns` | `traf.trun()` / `trun_mut()` (first), `traf.truns()` / `truns_mut()` |
//! | `trun.Duration(def)`, `trun.SampleCount()` | [`trun.duration(def)`](Trun::duration), `trun.sample_count()` |
//! | `trun.HasDataOffset()`, `trun.DataOffset += d` | `trun.has_data_offset()`, `trun.data_offset += d` |
//! | `frag.Moof.Traf.Senc != nil` | `traf.senc().is_some()` |
//! | `frag.Encode(w)` | [`frag.to_bytes()`](Fragment::to_bytes) / `encode_into(&mut buf)`; takes `&mut self` because, like mp4ff, it first sets the data offset of a lone trun ([`Fragment::set_trun_data_offsets`]) |
//!
//! **Decryption**
//!
//! | mp4ff | rustet-mp4 |
//! |---|---|
//! | `mp4.DecryptInit(init)` → `mp4.DecryptInfo` | [`decrypt_init(&mut init)`](decrypt_init) → [`DecryptInfo`] |
//! | `decryptInfo.TrackInfos`, `ti.TrackID`, `ti.Trex` | [`info.tracks`](DecryptInfo::tracks), `ti.track_id`, `ti.trex` |
//! | `ti.Sinf == nil` (clear track) | `ti.protection.is_none()` |
//! | `ti.Sinf.Schm.SchemeType` | `protection.scheme` ([`Scheme::Cenc`] / [`Scheme::Cbcs`]; other schemes fail in `decrypt_init`) |
//! | `ti.Sinf.Schi.Tenc` (+ nil checks) | [`protection.tenc()`](Protection::tenc) → `Option<&Tenc>` |
//! | `tenc.DefaultCryptByteBlock`, `.DefaultSkipByteBlock`, `.DefaultPerSampleIVSize`, `.DefaultConstantIV` | `tenc.default_crypt_byte_block`, `default_skip_byte_block`, `default_per_sample_iv_size`, `default_constant_iv` |
//! | `map[trackID]DecryptTrackInfo` built from `TrackInfos` (last entry wins) | `info.tracks.iter().rev().find(\|t\| t.track_id == id)`; [`info.track(id)`](DecryptInfo::track) is mp4ff's first-match `findTrackInfo` |
//! | `mp4.DecryptFragment(frag, di, key)` | [`decrypt_fragment(&mut frag, &info, &key)`](decrypt_fragment) |
//! | `isNoSencBox(err)` / `ErrNoSencBox` | `matches!(err, Error::NoSenc)` |
//! | `traf.ContainsSencBox()` | `traf.senc().is_some()` (parsing is stateless, so there is no "parsed" flag) |
//! | `senc.ParseReadBox(ivSize, traf.Saiz)` | [`senc.parse(iv_size)`](Senc::parse) → [`SencSamples`] |
//! | `traf.ParseReadSenc(ivSize, moof.StartPos)` (saio / seig aware) | [`traf.parse_senc(iv_size, moof.pos)`](Traf::parse_senc) |
//! | `senc.SubSamples[i]`, `ss.BytesOfClearData` / `.BytesOfProtectedData` | `samples.subsamples[i]`, `ss.clear` / `ss.protected` |
//! | `frag.GetFullSamples(ti.Trex)`, `samples[i].Data` | [`frag.full_samples(ti.trex.as_ref())`](Fragment::full_samples), `&mut mdat.data[s.data.clone()]` |
//! | `traf.RemoveEncryptionBoxes()` | [`traf.remove_encryption_boxes()`](Traf::remove_encryption_boxes) → bytes removed |
//! | `moof.RemovePsshs()` | [`moof.remove_psshs()`](Moof::remove_psshs) → bytes removed |
//! | `mp4.DecryptSampleCbcs` / `mp4.CryptSampleCenc` | [`decrypt_sample_cbcs`] / [`decrypt_sample_cenc`] |
//!
//! **Test fixtures** (feature `test-util`)
//!
//! | mp4ff | rustet-mp4 |
//! |---|---|
//! | `mp4.CreateEmptyInit(); init.AddEmptyTrack(ts, "video", "und")` | `InitSegment::single_track(ts, MediaType::Video)` |
//! | `mp4.CreateFragment(seq, trackID)` | `Fragment::new(seq, track_id)` |
//! | `frag.AddFullSample(mp4.FullSample{Sample: s, DecodeTime: t, Data: d})` | `frag.add_full_sample(TrunSample { .. }, t, &d)` |
//! | `mp4.CreateTrun(n)`; `traf.AddChild(trun)`; `trun.AddSample(s)` | `Trun::new()`; `traf.push(trun)`; `trun.samples.push(s)` |
//!
//! Encrypted fixtures (`mp4.InitProtect` / `mp4.EncryptFragment`) have no equivalent:
//! generate them once with mp4ff and commit the bytes.

#![forbid(unsafe_code)]

mod boxes;
#[cfg(feature = "test-util")]
mod build;
mod crypto;
mod error;
mod fourcc;
mod read;
mod segment;

pub use boxes::{
    BoxHeader, BoxType, Children, Container, Frma, Hdlr, Mdat, Mdhd, Mfhd, Moof, Moov, Mp4Box,
    Mvhd, RawBox, Saio, SampleEntry, Sbgp, Schm, Senc, SencSamples, Sgpd, Stsd, SubSample, Tenc,
    Tfdt, Tfhd, Tkhd, Traf, Trak, Trex, Trun, TrunSample, is_encryption_group,
};
#[cfg(feature = "test-util")]
pub use build::MediaType;
pub use crypto::{
    DecryptInfo, Protection, Scheme, TrackInfo, decrypt_fragment, decrypt_init,
    decrypt_sample_cbcs, decrypt_sample_cenc,
};
pub use error::{Error, Result};
pub use fourcc::FourCC;
pub use read::{read_box, read_box_async};
pub use segment::{Fragment, FullSample, InitSegment, Trim};
