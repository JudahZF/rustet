#!/usr/bin/env python3
"""Static signed Nix HTTP binary cache builder for GitHub Pages.

Subcommands:
  export-slice  Export store paths not already on cache.nixos.org into a
                signed binary-cache slice (narinfo + zstd NAR + slice.json).
  merge-site    Merge arch slices with recent entries from the live cache
                manifest and emit a ready-to-deploy GitHub Pages directory.
"""

from __future__ import annotations

import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
import datetime as dt
import hashlib
import html
import json
import os
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

NIX32_CHARS = "0123456789abcdfghijklmnpqrsvwxyz"


def to_nix32(digest: bytes) -> str:
    """Encode raw hash bytes in Nix's custom base32 alphabet."""
    length = (len(digest) * 8 - 1) // 5 + 1
    out = []
    for n in range(length - 1, -1, -1):
        b = n * 5
        i = b // 8
        j = b % 8
        c = digest[i] >> j
        if i + 1 < len(digest):
            c |= digest[i + 1] << (8 - j)
        out.append(NIX32_CHARS[c & 0x1F])
    return "".join(out)


def sri_to_nix32(sri: str) -> str:
    """Convert an SRI hash ('sha256-<base64>') or 'sha256:<nix32>' to 'sha256:<nix32>'."""
    if sri.startswith("sha256:"):
        return sri
    if not sri.startswith("sha256-"):
        raise ValueError(f"unsupported hash format: {sri}")
    raw = base64.b64decode(sri.split("-", 1)[1])
    return f"sha256:{to_nix32(raw)}"


def sha256_file_nix32(path: Path) -> tuple[str, int]:
    h = hashlib.sha256()
    size = 0
    with path.open("rb") as f:
        while True:
            chunk = f.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
            size += len(chunk)
    return f"sha256:{to_nix32(h.digest())}", size


def store_hash_of(store_path: str) -> str:
    return os.path.basename(store_path).split("-", 1)[0]


def on_upstream_cache(upstream_url: str, store_hash: str) -> bool:
    url = f"{upstream_url.rstrip('/')}/{store_hash}.narinfo"
    req = urllib.request.Request(url, method="HEAD", headers={"User-Agent": "rustet-nix-cache/1"})
    for _ in range(3):
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                return resp.status == 200
        except urllib.error.HTTPError as exc:
            if exc.code == 404:
                return False
        except urllib.error.URLError:
            pass
    return False


def nix_path_info(store_path: str) -> dict:
    raw = subprocess.check_output(["nix", "path-info", "--json", store_path], text=True)
    data = json.loads(raw)
    # Nix >= 2.30 (--json-format 2) may wrap entries under {"info": {...}}
    if isinstance(data, dict) and "info" in data and isinstance(data["info"], dict):
        return next(iter(data["info"].values()))
    if isinstance(data, dict):
        return data[store_path]
    if isinstance(data, list):
        return data[0]
    raise RuntimeError(f"unexpected nix path-info output for {store_path}: {type(data)}")


def format_narinfo(
    store_path: str,
    nar_rel_url: str,
    file_hash: str,
    file_size: int,
    nar_hash: str,
    nar_size: int,
    references: list[str],
    deriver: str | None,
    sig: str,
    ca: str | None,
) -> str:
    lines = [
        f"StorePath: {store_path}",
        f"URL: {nar_rel_url}",
        "Compression: zstd",
        f"FileHash: {file_hash}",
        f"FileSize: {file_size}",
        f"NarHash: {nar_hash}",
        f"NarSize: {nar_size}",
    ]
    refs = sorted(os.path.basename(r) for r in references)
    if refs:
        lines.append(f"References: {' '.join(refs)}")
    else:
        lines.append("References:")
    if deriver:
        lines.append(f"Deriver: {os.path.basename(deriver)}")
    if ca:
        lines.append(f"CA: {ca}")
    lines.append(f"Sig: {sig}")
    return "\n".join(lines) + "\n"


def export_one_path(
    store_path: str,
    key_file: Path,
    key_name: str,
    out_dir: Path,
) -> dict:
    subprocess.run(["nix", "store", "sign", "--key-file", str(key_file), store_path], check=True)
    info = nix_path_info(store_path)

    sigs = [s for s in info.get("signatures", []) if s.startswith(f"{key_name}:")]
    if not sigs:
        raise RuntimeError(f"signature for key {key_name!r} missing after signing {store_path}")
    sig = sigs[-1]

    nar_hash = sri_to_nix32(info["narHash"])
    nar_size = int(info["narSize"])
    references = info.get("references", [])
    deriver = info.get("deriver")
    ca = info.get("ca")

    nar_dir = out_dir / "nar"
    nar_dir.mkdir(parents=True, exist_ok=True)

    with tempfile.NamedTemporaryFile(dir=nar_dir, suffix=".nar.zst.tmp", delete=False) as tmp:
        tmp_path = Path(tmp.name)

    try:
        dump = subprocess.Popen(["nix-store", "--dump", store_path], stdout=subprocess.PIPE)
        assert dump.stdout is not None
        zstd = subprocess.Popen(
            ["zstd", "-19", "-T0", "-q", "-f", "-o", str(tmp_path)],
            stdin=dump.stdout,
        )
        dump.stdout.close()
        zstd_rc = zstd.wait()
        dump_rc = dump.wait()
        if dump_rc != 0 or zstd_rc != 0:
            raise RuntimeError(f"NAR compression failed (dump={dump_rc}, zstd={zstd_rc})")

        file_hash, file_size = sha256_file_nix32(tmp_path)
        file_hash_b32 = file_hash.split(":", 1)[1]
        nar_rel_url = f"nar/{file_hash_b32}.nar.zst"
        final_nar = out_dir / nar_rel_url
        tmp_path.replace(final_nar)
    except BaseException:
        tmp_path.unlink(missing_ok=True)
        raise

    sh = store_hash_of(store_path)
    narinfo_text = format_narinfo(
        store_path=store_path,
        nar_rel_url=nar_rel_url,
        file_hash=file_hash,
        file_size=file_size,
        nar_hash=nar_hash,
        nar_size=nar_size,
        references=references,
        deriver=deriver,
        sig=sig,
        ca=ca,
    )
    (out_dir / f"{sh}.narinfo").write_text(narinfo_text, encoding="utf-8")

    return {
        "store_hash": sh,
        "store_path": store_path,
        "nar_url": nar_rel_url,
        "file_hash": file_hash,
        "file_size": file_size,
        "nar_size": nar_size,
    }


def cmd_export_slice(args: argparse.Namespace) -> int:
    store_path = os.path.realpath(args.store_path)
    key_file = Path(args.key_file)
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    closure = subprocess.check_output(["nix", "path-info", "-r", store_path], text=True).splitlines()
    closure = [p.strip() for p in closure if p.strip()]

    others = [sp for sp in closure if sp != store_path]
    with ThreadPoolExecutor(max_workers=16) as pool:
        cached = list(pool.map(lambda sp: on_upstream_cache(args.upstream_cache, store_hash_of(sp)), others))
    to_export = [store_path, *(sp for sp, is_cached in zip(others, cached) if not is_cached)]

    print(f"Exporting {len(to_export)} of {len(closure)} closure paths for {args.arch}:", file=sys.stderr)
    exported = []
    for sp in to_export:
        print(f"  + {sp}", file=sys.stderr)
        exported.append(export_one_path(sp, key_file, args.key_name, out_dir))

    (out_dir / "nix-cache-info").write_text(
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n",
        encoding="utf-8",
    )

    if args.public_key:
        for item in exported:
            subprocess.run(
                [
                    "nix",
                    "store",
                    "verify",
                    "--store",
                    f"file://{out_dir.resolve()}",
                    "--trusted-public-keys",
                    args.public_key,
                    item["store_path"],
                ],
                check=True,
            )

    slice_meta = {
        "arch": args.arch,
        "commit": args.commit,
        "built_at": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "root_path": store_path,
        "paths": exported,
    }
    (out_dir / "slice.json").write_text(json.dumps(slice_meta, indent=2) + "\n", encoding="utf-8")
    return 0


def fetch_url_bytes(url: str, timeout: int = 60) -> bytes | None:
    req = urllib.request.Request(url, headers={"User-Agent": "rustet-nix-cache/1"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            if resp.status == 200:
                return resp.read()
    except (urllib.error.HTTPError, urllib.error.URLError) as exc:
        print(f"Note: could not fetch {url}: {exc}", file=sys.stderr)
    return None


def parse_narinfo_field(text: str, key: str) -> str | None:
    prefix = f"{key}: "
    for line in text.splitlines():
        if line.startswith(prefix):
            return line[len(prefix) :].strip()
    return None


def render_index_html(cache_url: str, public_key: str, manifest: dict) -> str:
    rows = []
    for arch in sorted(manifest.get("builds", {}).keys()):
        for build in manifest["builds"][arch]:
            root = os.path.basename(build["root_path"])
            commit = build.get("commit", "")[:12]
            built_at = build.get("built_at", "")
            total_mb = sum(p.get("file_size", 0) for p in build.get("paths", [])) / (1024 * 1024)
            rows.append(
                f"<tr><td><code>{html.escape(arch)}</code></td>"
                f"<td><code>{html.escape(commit)}</code></td>"
                f"<td><code>{html.escape(root)}</code></td>"
                f"<td>{total_mb:.1f} MiB</td>"
                f"<td>{html.escape(built_at)}</td></tr>"
            )
    rows_html = "\n        ".join(rows) if rows else "<tr><td colspan='5'>No cached builds yet.</td></tr>"
    return f"""<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Rustet Nix Binary Cache</title>
  <style>
    body {{ font-family: system-ui, -apple-system, sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; color: #1f2328; }}
    pre, code {{ background: #f6f8fa; border-radius: 6px; font-family: ui-monospace, SFMono-Regular, monospace; }}
    code {{ padding: 0.15em 0.35em; font-size: 0.9em; }}
    pre {{ padding: 1rem; overflow-x: auto; }}
    table {{ border-collapse: collapse; width: 100%; margin-top: 1rem; }}
    th, td {{ border: 1px solid #d0d7de; padding: 0.5rem 0.75rem; text-align: left; }}
    th {{ background: #f6f8fa; }}
  </style>
</head>
<body>
  <h1>Rustet Nix Binary Cache</h1>
  <p>Signed binary cache for <a href="https://github.com/JudahZF/rustet">JudahZF/rustet</a> (<code>x86_64-linux</code> and <code>aarch64-linux</code>).</p>
  <h2>NixOS configuration</h2>
  <pre><code>nix.settings = {{
  extra-substituters = [ "{html.escape(cache_url)}" ];
  extra-trusted-public-keys = [ "{html.escape(public_key)}" ];
}};</code></pre>
  <h2>Cached builds</h2>
  <table>
    <thead>
      <tr><th>Architecture</th><th>Commit</th><th>Store Path</th><th>Compressed Size</th><th>Built (UTC)</th></tr>
    </thead>
    <tbody>
      {rows_html}
    </tbody>
  </table>
</body>
</html>
"""


def cmd_merge_site(args: argparse.Namespace) -> int:
    site_dir = Path(args.site_dir)
    nar_dir = site_dir / "nar"
    nar_dir.mkdir(parents=True, exist_ok=True)
    cache_url = args.cache_url.rstrip("/")
    key_name = args.public_key.split(":", 1)[0]

    new_slices: list[dict] = []
    local_files: dict[str, Path] = {}
    for slice_arg in args.slice:
        sdir = Path(slice_arg)
        meta = json.loads((sdir / "slice.json").read_text(encoding="utf-8"))
        new_slices.append(meta)
        for p in meta["paths"]:
            ni = f"{p['store_hash']}.narinfo"
            local_files[ni] = sdir / ni
            local_files[p["nar_url"]] = sdir / p["nar_url"]

    prev_manifest: dict = {"builds": {}}
    if not args.no_fetch_existing:
        raw_manifest = fetch_url_bytes(f"{cache_url}/manifest.json", timeout=30)
        if raw_manifest:
            try:
                prev_manifest = json.loads(raw_manifest.decode("utf-8"))
            except json.JSONDecodeError as exc:
                print(f"Warning: ignoring invalid remote manifest.json: {exc}", file=sys.stderr)

    merged_builds: dict[str, list[dict]] = {}
    # Start with previous builds grouped by arch
    for arch, builds in prev_manifest.get("builds", {}).items():
        merged_builds[arch] = list(builds)

    # Prepend new builds, deduplicating by root_path
    for s in new_slices:
        arch = s["arch"]
        existing = [b for b in merged_builds.get(arch, []) if b.get("root_path") != s["root_path"]]
        merged_builds[arch] = [s, *existing][: args.max_per_arch]

    # Materialize every retained build into site_dir
    final_builds: dict[str, list[dict]] = {}
    for arch, builds in merged_builds.items():
        kept: list[dict] = []
        for build in builds:
            ok = True
            for p in build["paths"]:
                ni_rel = f"{p['store_hash']}.narinfo"
                nar_rel = p["nar_url"]
                ni_dst = site_dir / ni_rel
                nar_dst = site_dir / nar_rel

                if ni_rel in local_files and nar_rel in local_files:
                    shutil.copy2(local_files[ni_rel], ni_dst)
                    nar_dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(local_files[nar_rel], nar_dst)
                    continue

                if ni_dst.exists() and nar_dst.exists():
                    continue

                ni_bytes = fetch_url_bytes(f"{cache_url}/{ni_rel}", timeout=30)
                nar_bytes = fetch_url_bytes(f"{cache_url}/{nar_rel}", timeout=120) if ni_bytes else None
                if not ni_bytes or not nar_bytes:
                    print(f"Dropping stale cached build {build['root_path']} (missing remote files)", file=sys.stderr)
                    ok = False
                    break

                ni_text = ni_bytes.decode("utf-8")
                expected_hash = parse_narinfo_field(ni_text, "FileHash")
                actual_hash = f"sha256:{to_nix32(hashlib.sha256(nar_bytes).digest())}"
                if expected_hash != actual_hash:
                    print(
                        f"Dropping corrupted cached build {build['root_path']} "
                        f"(expected {expected_hash}, got {actual_hash})",
                        file=sys.stderr,
                    )
                    ok = False
                    break

                ni_dst.write_bytes(ni_bytes)
                nar_dst.parent.mkdir(parents=True, exist_ok=True)
                nar_dst.write_bytes(nar_bytes)

            if ok:
                kept.append(build)
        if kept:
            final_builds[arch] = kept

    # Verify every retained path is present and signed by our key
    for arch, builds in final_builds.items():
        for build in builds:
            for p in build["paths"]:
                ni_path = site_dir / f"{p['store_hash']}.narinfo"
                nar_path = site_dir / p["nar_url"]
                if not ni_path.is_file() or not nar_path.is_file():
                    raise RuntimeError(f"missing output file for {p['store_path']}")
                sig = parse_narinfo_field(ni_path.read_text(encoding="utf-8"), "Sig")
                if not sig or not sig.startswith(f"{key_name}:"):
                    raise RuntimeError(f"missing {key_name} signature in {ni_path}")

    manifest = {
        "cache_url": cache_url,
        "public_key": args.public_key,
        "updated_at": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "builds": final_builds,
    }
    (site_dir / ".nojekyll").write_text("", encoding="utf-8")
    (site_dir / "nix-cache-info").write_text(
        "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n",
        encoding="utf-8",
    )
    (site_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (site_dir / "index.html").write_text(
        render_index_html(cache_url, args.public_key, manifest),
        encoding="utf-8",
    )

    total_builds = sum(len(v) for v in final_builds.values())
    total_bytes = sum(f.stat().st_size for f in site_dir.rglob("*") if f.is_file())
    print(f"Built site in {site_dir}: {total_builds} build(s), {total_bytes / (1024 * 1024):.1f} MiB", file=sys.stderr)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Static signed Nix HTTP binary cache builder")
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_exp = sub.add_parser("export-slice", help="Export uncached closure paths to a signed slice")
    p_exp.add_argument("--store-path", required=True)
    p_exp.add_argument("--key-file", required=True)
    p_exp.add_argument("--key-name", required=True)
    p_exp.add_argument("--public-key", default="")
    p_exp.add_argument("--out-dir", required=True)
    p_exp.add_argument("--arch", required=True)
    p_exp.add_argument("--commit", required=True)
    p_exp.add_argument("--upstream-cache", default="https://cache.nixos.org")
    p_exp.set_defaults(func=cmd_export_slice)

    p_merge = sub.add_parser("merge-site", help="Merge slices and previous builds into a Pages site")
    p_merge.add_argument("--site-dir", required=True)
    p_merge.add_argument("--slice", action="append", required=True)
    p_merge.add_argument("--cache-url", required=True)
    p_merge.add_argument("--public-key", required=True)
    p_merge.add_argument("--max-per-arch", type=int, default=8)
    p_merge.add_argument("--no-fetch-existing", action="store_true")
    p_merge.set_defaults(func=cmd_merge_site)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
