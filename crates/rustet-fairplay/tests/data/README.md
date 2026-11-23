# Fixtures

Made once with a throwaway Go program against the Go engine (`engine/core/fairplay`) and
mp4ff (`itouakirai/mp4ff v0.0.0-20250930132656-98812935a1c7`). The program is not kept.

| File | Content |
|---|---|
| `cbcs_track.mp4` | mp4ff `InitProtect` + `EncryptFragment`: AAC 44.1 kHz, `cbcs`, constant IV `d1…d1`, 5 fragments of 4 samples (4096 ticks each, 200–499 bytes), fragment 0 under key `11…11`, fragments 1–4 under key `22…22`, every tfdt 0 (as Apple ships them) |
| `cbcs_track.plain` | The samples of `cbcs_track.mp4` in the clear, in order, each as `[u32 BE length][bytes]` |
| `cbcs_audio.in.mp4` | Copy of `crates/rustet-mp4/tests/data/cbcs_audio.in.mp4` (mp4ff test data, MIT): AAC `cbcs`, 7 truns per traf, key `5ffd93861fa776e96cccd934898fc1c8` |
| `go_full.mp4` | Go `CBCSSeekableSource(…, 0).Stream` over `cbcs_track.mp4`, keys `[prefetch, content]` |
| `go_full_dur.mp4` | The same with `durationMs` 2322 |
| `go_seek2_dur.mp4` | Go `SourceFrom(2.5 × segment)` (starts at fragment 2), `durationMs` 2322 |
| `go_seek0.mp4` | Go `SourceFrom(0.5 × segment)` (seek path from fragment 0), `durationMs` 0 |
| `go_multitrun.mp4` | Go `CBCSSource(…, ["skd://aac"], 0).Stream` over `cbcs_audio.in.mp4` |

The fake decryptor of the Go runs decrypted each packet with AES-128-CBC from the constant
IV, like `tests/common/mod.rs`.
