# Fixtures

| File | Content |
|---|---|
| `identity.pem` | A loopback TLS identity made with OpenSSL 3.5 (`openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes …`, key converted with `openssl pkcs8 -topk8 -nocrypt`): certificate block, then PKCS#8 key block |
| `identity.spki-sha256` | `openssl x509 -in identity.pem -pubkey -noout \| openssl pkey -pubin -outform der \| openssl dgst -sha256 -binary \| base64` |
| `direct_init.mp4`, `direct_seg.m4s`, `direct_next.m4s` | Copies of `crates/rustet-streams/tests/data`: a clear fMP4 video init (timescale 24) and seven one-second fragments on a raw timeline starting at 10 s (mp4ff-built, from the Go engine's direct-seek tests) |
