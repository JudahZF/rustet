#!/usr/bin/env python3
"""Records a static catalog for the rustet-ui proof: 5,000 albums and 10,000 tracks with real
Apple artwork CDN URLs, from the public iTunes Search API (no account). Storefronts in several
scripts (Japan, Korea, Taiwan, Saudi Arabia, Israel, India, Russia, Greece, Thailand) give real
CJK, Arabic, Hebrew, Devanagari, Cyrillic, Greek and Thai metadata.

    record-catalog.py OUT.json

Artwork URLs are stored as templates ("…/{w}x{h}bb.jpg"), as the Apple Music API returns them.
"""
import json
import sys
import time
import urllib.parse
import urllib.request

STOREFRONTS = ["us", "jp", "kr", "tw", "sa", "il", "in", "ru", "gr", "th", "gb", "br", "fr", "de"]
TERMS = ["love", "night", "live", "best", "song", "heart", "dance", "world", "blue", "rain",
         "dream", "fire", "home", "time", "star", "summer", "moon", "light", "road", "city"]
ALBUMS, TRACKS = 5000, 10000


def search(term, country, entity):
    q = urllib.parse.urlencode({"term": term, "country": country, "entity": entity, "limit": 200})
    for attempt in range(4):
        try:
            with urllib.request.urlopen(f"https://itunes.apple.com/search?{q}", timeout=30) as r:
                return json.load(r)["results"]
        except Exception as e:  # rate limit (403/429) or network: back off
            print(f"  {term}/{country}/{entity}: {e}; retry", file=sys.stderr)
            time.sleep(20 * (attempt + 1))
    return []


def template(url):
    return url.rsplit("/", 1)[0] + "/{w}x{h}bb.jpg" if url else None


def main(out):
    albums, tracks = {}, {}
    for term in TERMS:
        for country in STOREFRONTS:
            if len(albums) >= ALBUMS and len(tracks) >= TRACKS:
                break
            if len(albums) < ALBUMS:
                for r in search(term, country, "album"):
                    if r.get("artworkUrl100"):
                        albums.setdefault(r["collectionId"], {
                            "id": str(r["collectionId"]), "name": r["collectionName"],
                            "artist": r["artistName"], "artwork": template(r["artworkUrl100"]),
                            "year": (r.get("releaseDate") or "")[:4], "country": country})
                time.sleep(3.2)
            if len(tracks) < TRACKS:
                for r in search(term, country, "song"):
                    tracks.setdefault(r["trackId"], {
                        "id": str(r["trackId"]), "name": r["trackName"], "artist": r["artistName"],
                        "album": r.get("collectionName", ""), "durationMs": r.get("trackTimeMillis", 0),
                        "artwork": template(r.get("artworkUrl100")), "country": country})
                time.sleep(3.2)
            print(f"{term}/{country}: {len(albums)} albums, {len(tracks)} tracks", file=sys.stderr)
    data = {"albums": list(albums.values())[:ALBUMS], "tracks": list(tracks.values())[:TRACKS]}
    with open(out, "w") as f:
        json.dump(data, f, ensure_ascii=False)


if __name__ == "__main__":
    main(sys.argv[1])
