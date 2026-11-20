#!/usr/bin/env python3
# SPDX-License-Identifier: BSD-2-Clause
"""Extract the six Profile 0 folders from the official Argon Streams AV1 v2.1.1 ZIP."""

import argparse
from collections import Counter
from contextlib import nullcontext
import hashlib
import http.client
import io
from pathlib import Path, PurePosixPath
import re
import shutil
import sys
import time
import urllib.request
import zipfile

from suite import FOLDERS

URL = (
    "https://aom-cwg-av1-argon-streams-public.s3.us-east-1.amazonaws.com/"
    "argon_coveragetool_av1_base_and_extended_profiles_v2.1.1.zip"
)
ARCHIVE_SIZE = 7_028_947_374
ARCHIVE_MD5 = "5dd3554c3f73c80f06c7fceeaa1f5dd9"
ARCHIVE_ETAG = '"6c00e64ca1159188b1b5312c2b65156b-410"'
CHUNK = 8 << 20


class RemoteZip(io.RawIOBase):
    """Seek over the ZIP using validated HTTP byte ranges."""

    def __init__(self):
        request = urllib.request.Request(URL, method="HEAD")
        with urllib.request.urlopen(request, timeout=60) as response:
            if (int(response.headers["Content-Length"]), response.headers["ETag"]) != (ARCHIVE_SIZE, ARCHIVE_ETAG):
                raise ValueError("S3 object does not match the pinned Argon release")
            checksum = response.headers.get("x-amz-meta-md5")
            if checksum is not None and checksum.lower() != ARCHIVE_MD5:
                raise ValueError("S3 object's MD5 metadata does not match the published Argon MD5")
        self.position = 0
        self.window_end = None
        self.buffer_start = 0
        self.buffer = b""
        self.downloaded = 0

    def seekable(self):
        return True

    def readable(self):
        return True

    def tell(self):
        return self.position

    def seek(self, offset, whence=io.SEEK_SET):
        base = {io.SEEK_SET: 0, io.SEEK_CUR: self.position, io.SEEK_END: ARCHIVE_SIZE}[whence]
        self.position = base + offset
        if self.position < 0:
            raise OSError("negative ZIP offset")
        return self.position

    def fetch(self, start, end):
        request = urllib.request.Request(URL, headers={
            "Range": f"bytes={start}-{end - 1}",
            "If-Match": ARCHIVE_ETAG,
        })
        for attempt in range(5):
            try:
                with urllib.request.urlopen(request, timeout=120) as response:
                    if (response.status != 206 or response.headers["ETag"] != ARCHIVE_ETAG
                            or response.headers["Content-Range"] != f"bytes {start}-{end - 1}/{ARCHIVE_SIZE}"):
                        raise ValueError("S3 did not return the requested Argon ZIP range")
                    data = response.read(end - start)
                    if len(data) != end - start:
                        raise OSError("truncated Argon ZIP range")
                    self.downloaded += len(data)
                    return data
            except (OSError, http.client.HTTPException) as error:
                if attempt == 4:
                    raise
                print(f"Retrying ZIP range {start}-{end - 1} after {error}", file=sys.stderr)
                time.sleep(2 ** attempt)

    def read(self, size=-1):
        left = max(0, ARCHIVE_SIZE - self.position)
        size = left if size < 0 else min(size, left)
        if not size:
            return b""
        start = self.position
        end = start + size
        if not (self.buffer_start <= start and end <= self.buffer_start + len(self.buffer)):
            if self.window_end is not None:
                if end > self.window_end:
                    raise ValueError("ZIP member extends into an unselected folder")
                fetch_end = max(end, min(start + CHUNK, self.window_end))
            else:
                fetch_end = end
            self.buffer = self.fetch(start, fetch_end)
            self.buffer_start = start
        self.position = end
        return self.buffer[start - self.buffer_start:end - self.buffer_start]


def profile_path(name):
    path = PurePosixPath(name)
    if len(path.parts) < 2 or path.parts[1] not in FOLDERS:
        return None
    if any(part in (".", "..") or not re.fullmatch(r"[A-Za-z0-9_.-]+", part) for part in path.parts):
        raise ValueError(f"unsafe Argon member name {name!r}")
    return Path(*path.parts[1:])


def selections(infos):
    """Yield blocks of adjacent selected ZIP members, with the next local header as their range limit."""
    block = []
    for info in sorted(infos, key=lambda info: info.header_offset):
        relative = profile_path(info.filename)
        if relative is None:
            if block:
                yield info.header_offset, block
                block = []
        elif not info.is_dir():
            block.append((info, relative))
    if block:
        yield None, block


def verify_local(path):
    with path.open("rb") as file:
        actual = hashlib.file_digest(file, lambda: hashlib.md5(usedforsecurity=False)).hexdigest()
    if actual != ARCHIVE_MD5:
        raise ValueError(f"ZIP MD5 {actual} differs from {ARCHIVE_MD5}")
    print(f"Verified published ZIP MD5 {actual}")


def extract(source, out):
    with zipfile.ZipFile(source) as archive:
        blocks = list(selections(archive.infolist()))
        paths = [relative for _, block in blocks for _, relative in block]
        if len(paths) != len(set(paths)):
            raise ValueError("duplicate paths in Profile 0 ZIP members")
        counts = Counter(path.parts[0] for path in paths if len(path.parts) == 3
                         and path.parts[1] == "streams" and path.suffix == ".obu")
        if counts != FOLDERS:
            raise ValueError(f"ZIP does not contain the expected Profile 0 inventory: {dict(counts)}")
        total = sum(info.file_size for _, block in blocks for info, _ in block)
        print(f"Extracting {len(paths)} files ({total:,} bytes) from the six Profile 0 folders", flush=True)
        for end, block in blocks:
            if isinstance(source, RemoteZip):
                source.window_end = end if end is not None else archive.start_dir
            for info, relative in block:
                target = out / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                partial = target.with_name(target.name + ".part")
                try:
                    # Reading through EOF performs zipfile's CRC32 check for this member.
                    with archive.open(info) as contents, partial.open("wb") as file:
                        shutil.copyfileobj(contents, file, length=1 << 20)
                    partial.replace(target)
                finally:
                    partial.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--zip", type=Path, help="use an existing full archive and verify its published MD5")
    args = parser.parse_args()
    try:
        if args.zip:
            verify_local(args.zip)
            source = nullcontext(args.zip)
        else:
            source = RemoteZip()
            print("Range mode: checked the S3 size and multipart ETag. ZIP members are CRC32-checked individually.")
        with source as archive:
            extract(archive, args.out)
            if isinstance(archive, RemoteZip):
                print(f"Transferred {archive.downloaded:,} bytes of ZIP data and metadata")
    except (OSError, ValueError, zipfile.BadZipFile, http.client.HTTPException) as error:
        print(f"Argon fetch: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
