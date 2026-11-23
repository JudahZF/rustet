# Fixtures

Cut from the mp4ff test data (`github.com/Eyevinn/mp4ff`, MIT licence, `mp4/testdata`):
the `ftyp` + `moov` and the first two `moof` + `mdat` fragments of each file.

| File | Source | Content | Key |
|---|---|---|---|
| `cbcs_audio.in.mp4` | `cbcs_audio.mp4` | AAC, `cbcs`, constant IV, 7 truns per traf | `5ffd93861fa776e96cccd934898fc1c8` |
| `cbcs_video.in.mp4` | `cbcs.mp4` | AVC (`cbcs` 1:9 pattern) + clear `c608` track | `22bdb0063805260307ee5045c0f3835a` |
| `cenc_video.in.mp4` | `prog_8s_enc_dashinit.mp4` | AVC + AAC, `cenc`, 8-byte IVs, two trafs per moof | `63cb5f7184dd4b689a5c5ff11ee6a328` |

The SHA-256 values in `tests/mp4ff_parity.rs` are of the output of mp4ff
(`itouakirai/mp4ff v0.0.0-20250930132656-98812935a1c7`) for the same operations.
