#!/usr/bin/env python3
"""Redact local paths from BEP fixture streams without changing wire lengths."""

import argparse
import pathlib
import re


def fixed_placeholder(label: str, length: int) -> bytes:
    prefix = f"<{label}>".encode()
    if len(prefix) > length:
        raise ValueError(f"placeholder {prefix!r} is longer than source ({length})")
    return prefix + b"_" * (length - len(prefix))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("--replace", action="append", default=[], metavar="LABEL=PATH")
    parser.add_argument("--text", action="store_true")
    args = parser.parse_args()

    replacements = []
    for value in args.replace:
        label, separator, source = value.partition("=")
        if not separator or not label or not source:
            raise SystemExit(f"invalid replacement {value!r}; expected LABEL=PATH")
        replacements.append((label, source.encode()))

    data = args.input.read_bytes()
    for label, source in sorted(replacements, key=lambda pair: len(pair[1]), reverse=True):
        if args.text:
            data = data.replace(source, f"<{label}>".encode())
        else:
            data = data.replace(source, fixed_placeholder(label, len(source)))

    if args.text:
        # Bazel progress lines commonly retain a space after their final colon.
        # Keep checked text fixtures diff-clean and stable across editors.
        data = re.sub(rb"[ \t]+(?=\r?$)", b"", data, flags=re.MULTILINE)

    forbidden = [
        rb"/Users/[^/\x00\s]+",
        rb"/home/[^/\x00\s]+",
        rb"token=[^\x00\s]+",
        rb"(?i)(api[_-]?key|password|secret)=[^\x00\s]+",
    ]
    for pattern in forbidden:
        match = re.search(pattern, data)
        if match:
            raise SystemExit(f"fixture still contains forbidden material matching {pattern!r}")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(data)


if __name__ == "__main__":
    main()
