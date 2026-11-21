#!/usr/bin/env python3
"""Creates the deterministic fixture tree served by the test server.

Layout below the target directory:
  many/      100,000 files with realistic names, sizes (sparse) and dates
  tree/      a small nested tree with unicode, hidden and special entries
"""

import os
import random
import sys

MANY_COUNT = 100_000
SEED = 20261120
WORDS = [
    "report", "invoice", "IMG", "photo", "backup", "draft", "Notes", "data",
    "log", "video", "scan", "Budget", "export", "archive", "readme", "song",
    "Präsentation", "résumé", "chart", "dump",
]
EXTENSIONS = ["txt", "pdf", "jpg", "png", "mp4", "tar.gz", "csv", "json", "md", "log", ""]
OLDEST = 1_546_300_800  # 2019-01-01
NEWEST = 1_793_491_200  # 2026-11-01


def many(root: str) -> None:
    rng = random.Random(SEED)
    os.makedirs(root, exist_ok=True)
    for index in range(MANY_COUNT):
        word = rng.choice(WORDS)
        ext = rng.choice(EXTENSIONS)
        name = f"{word}-{index:06d}" + (f".{ext}" if ext else "")
        path = os.path.join(root, name)
        # Log-normal sizes from a few bytes to ~100 MB; sparse, so they cost no disk.
        size = min(int(rng.lognormvariate(9, 3)), 100 * 1024 * 1024)
        with open(path, "wb") as handle:
            handle.truncate(size)
        mtime = rng.randint(OLDEST, NEWEST)
        os.utime(path, (mtime, mtime))


def tree(root: str) -> None:
    os.makedirs(os.path.join(root, "nested", "deeper", "deepest"), exist_ok=True)
    os.makedirs(os.path.join(root, "empty"), exist_ok=True)
    files = {
        "hello.txt": b"Hello from the ferge test server.\n",
        "Ünïcödé ✓.txt": "Unicode name\n".encode(),
        "with space.txt": b"spaces\n",
        ".hidden": b"hidden\n",
        "nested/deeper/deepest/bottom.txt": b"bottom\n",
    }
    for relative, content in files.items():
        with open(os.path.join(root, relative), "wb") as handle:
            handle.write(content)
    os.symlink("nested", os.path.join(root, "link-to-nested"))
    os.symlink("hello.txt", os.path.join(root, "link-to-hello"))
    os.symlink("does-not-exist", os.path.join(root, "dangling-link"))


def main() -> None:
    home = sys.argv[1]
    many(os.path.join(home, "many"))
    tree(os.path.join(home, "tree"))


if __name__ == "__main__":
    main()
