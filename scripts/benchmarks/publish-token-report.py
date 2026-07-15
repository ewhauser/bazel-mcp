#!/usr/bin/env python3
"""Create a reviewable, deterministic benchmark snapshot for version control."""

import argparse
import gzip
import hashlib
import json
import pathlib
import shutil
import tarfile


def digest(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def deterministic_archive(
    source: pathlib.Path,
    destination: pathlib.Path,
    prefix: str,
    pattern: str,
    replacements: dict[bytes, bytes],
) -> list[dict]:
    records = []
    files = sorted(path for path in source.rglob(pattern) if path.is_file())
    with destination.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                for path in files:
                    relative = path.relative_to(source)
                    data = path.read_bytes()
                    for original, replacement in replacements.items():
                        data = data.replace(original, replacement)
                    reject_sensitive(data, f"{prefix}/{relative.as_posix()}")
                    info = tarfile.TarInfo(f"{prefix}/{relative.as_posix()}")
                    info.size = len(data)
                    info.mode = 0o644
                    info.mtime = 0
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    archive.addfile(info, __import__("io").BytesIO(data))
                    records.append(
                        {
                            "path": relative.as_posix(),
                            "bytes": len(data),
                            "sha256": hashlib.sha256(data).hexdigest(),
                        }
                    )
    return records


def reject_sensitive(data: bytes, label: str) -> None:
    lowered = data.lower()
    for marker in [
        b"/users/",
        b"/home/",
        b"authorization=",
        b"password=",
        b"token=",
        b"x-buildbuddy-api-key=",
    ]:
        if marker in lowered:
            raise SystemExit(f"sensitive marker {marker!r} in {label}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("destination", type=pathlib.Path)
    parser.add_argument("--replace", action="store_true")
    args = parser.parse_args()
    args.source = args.source.resolve()
    args.destination = args.destination.resolve()

    report_path = args.source / "report.json"
    markdown_path = args.source / "report.md"
    transcripts = args.source / "transcripts"
    evidence = args.source / "evidence"
    for required in [report_path, markdown_path, transcripts, evidence]:
        if not required.exists():
            raise SystemExit(f"missing benchmark artifact: {required}")
    report = json.loads(report_path.read_text())
    if report.get("schema_version", 0) < 3:
        raise SystemExit("only schema-v3-or-newer benchmark reports may be published")
    if len(report.get("comparisons", [])) != 2:
        raise SystemExit("report must compare MCP against default and optimized shell baselines")
    if args.destination.exists() and any(args.destination.iterdir()):
        if not args.replace:
            raise SystemExit(f"destination is not empty: {args.destination}")
        expected = {
            "report.json", "report.md", "transcripts.tar.gz", "evidence.tar.gz", "manifest.json"
        }
        existing = {path.name for path in args.destination.iterdir()}
        if not existing <= expected:
            raise SystemExit(f"destination contains unexpected files: {args.destination}")
        for path in args.destination.iterdir():
            path.unlink()

    args.destination.mkdir(parents=True, exist_ok=True)
    reject_sensitive(report_path.read_bytes(), "report.json")
    reject_sensitive(markdown_path.read_bytes(), "report.md")
    shutil.copyfile(report_path, args.destination / "report.json")
    shutil.copyfile(markdown_path, args.destination / "report.md")
    archive_path = args.destination / "transcripts.tar.gz"
    replacements = {str(args.source).encode(): b"<RUN_ROOT>"}
    records = deterministic_archive(
        transcripts, archive_path, "transcripts", "*.jsonl", replacements
    )
    evidence_archive_path = args.destination / "evidence.tar.gz"
    evidence_records = deterministic_archive(
        evidence, evidence_archive_path, "evidence", "*.log", replacements
    )
    manifest = {
        "schema_version": 1,
        "project": report["project"],
        "commit": report["commit"],
        "bazel_version": report["bazel_version"],
        "environment": report["environment"],
        "report_json_sha256": digest(args.destination / "report.json"),
        "report_markdown_sha256": digest(args.destination / "report.md"),
        "transcript_archive_sha256": digest(archive_path),
        "transcript_count": len(records),
        "transcripts": records,
        "evidence_archive_sha256": digest(evidence_archive_path),
        "evidence_count": len(evidence_records),
        "evidence": evidence_records,
    }
    (args.destination / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
