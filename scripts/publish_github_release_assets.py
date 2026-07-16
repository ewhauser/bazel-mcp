#!/usr/bin/env python3
"""Publish cargo-dist artifacts without GitHub CLI release discovery."""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode
from urllib.request import Request, urlopen

RETRYABLE_STATUSES = {0, 429, 500, 502, 503, 504}


class PublishError(RuntimeError):
    pass


@dataclass(frozen=True)
class HttpResult:
    status: int
    body: bytes

    def json(self) -> Any:
        return json.loads(self.body)


class GitHubClient:
    def __init__(
        self,
        token: str,
        *,
        max_attempts: int = 5,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        self.token = token
        self.max_attempts = max_attempts
        self.sleep = sleep

    def request_once(
        self,
        method: str,
        url: str,
        *,
        authenticated: bool = True,
        body: bytes | None = None,
        content_type: str = "application/vnd.github+json",
    ) -> HttpResult:
        headers = {
            "Accept": "application/vnd.github+json",
            "Content-Type": content_type,
            "X-GitHub-Api-Version": "2022-11-28",
        }
        if authenticated:
            headers["Authorization"] = f"Bearer {self.token}"
        request = Request(
            url,
            data=body,
            method=method,
            headers=headers,
        )
        try:
            with urlopen(request, timeout=120) as response:
                return HttpResult(response.status, response.read())
        except HTTPError as error:
            try:
                return HttpResult(error.code, error.read())
            finally:
                error.close()
        except URLError as error:
            return HttpResult(0, str(error).encode())

    def request(
        self,
        method: str,
        url: str,
        *,
        authenticated: bool = True,
        body: bytes | None = None,
        content_type: str = "application/vnd.github+json",
    ) -> HttpResult:
        for attempt in range(1, self.max_attempts + 1):
            result = self.request_once(
                method,
                url,
                authenticated=authenticated,
                body=body,
                content_type=content_type,
            )
            if result.status not in RETRYABLE_STATUSES or attempt == self.max_attempts:
                return result
            self.sleep(attempt * 3)
        raise AssertionError("unreachable")


class ReleasePublisher:
    def __init__(
        self,
        client: GitHubClient,
        repository: str,
        tag: str,
        *,
        api_url: str = "https://api.github.com",
    ) -> None:
        self.client = client
        self.repository = repository
        self.tag = tag
        self.api_url = api_url.rstrip("/")
        self.release_by_tag_url = (
            f"{self.api_url}/repos/{repository}/releases/tags/{quote(tag, safe='')}"
        )
        self.releases_url = f"{self.api_url}/repos/{repository}/releases"

    @staticmethod
    def _require(result: HttpResult, expected: set[int], operation: str) -> None:
        if result.status in expected:
            return
        detail = result.body.decode("utf-8", errors="replace")[:1000]
        raise PublishError(f"{operation} failed with HTTP {result.status}: {detail}")

    def lookup_release(self, *, authenticated: bool = False) -> dict[str, Any] | None:
        result = self.client.request(
            "GET", self.release_by_tag_url, authenticated=authenticated
        )
        if result.status == 404:
            return None
        self._require(result, {200}, "release lookup")
        return result.json()

    def create_draft_release(
        self, title: str, notes: str, prerelease: bool
    ) -> dict[str, Any]:
        payload = json.dumps(
            {
                "tag_name": self.tag,
                "name": title,
                "body": notes,
                "draft": True,
                "prerelease": prerelease,
            }
        ).encode()
        for attempt in range(1, self.client.max_attempts + 1):
            result = self.client.request_once("POST", self.releases_url, body=payload)
            if result.status == 201:
                return result.json()
            if result.status in RETRYABLE_STATUSES:
                existing = self.lookup_release(authenticated=True)
                if existing is not None:
                    return existing
                if attempt < self.client.max_attempts:
                    self.client.sleep(attempt * 3)
                    continue
            self._require(result, {201}, "draft release creation")
        raise AssertionError("unreachable")

    def list_assets(self, release_id: int) -> list[dict[str, Any]]:
        assets: list[dict[str, Any]] = []
        page = 1
        while True:
            url = f"{self.releases_url}/{release_id}/assets?" + urlencode(
                {"per_page": 100, "page": page}
            )
            result = self.client.request("GET", url, authenticated=False)
            self._require(result, {200}, "release asset listing")
            batch = result.json()
            assets.extend(batch)
            if len(batch) < 100:
                return assets
            page += 1

    def delete_asset(self, asset_id: int) -> None:
        url = f"{self.api_url}/repos/{self.repository}/releases/assets/{asset_id}"
        result = self.client.request("DELETE", url)
        self._require(result, {204, 404}, "release asset deletion")

    def upload_asset(self, release: dict[str, Any], artifact: Path) -> None:
        release_id = int(release["id"])
        for asset in self.list_assets(release_id):
            if asset["name"] == artifact.name:
                self.delete_asset(int(asset["id"]))

        upload_base = str(release["upload_url"]).split("{", 1)[0]
        upload_url = f"{upload_base}?{urlencode({'name': artifact.name})}"
        content = artifact.read_bytes()
        for attempt in range(1, self.client.max_attempts + 1):
            result = self.client.request_once(
                "POST",
                upload_url,
                body=content,
                content_type="application/octet-stream",
            )
            if result.status == 201:
                print(f"uploaded: {artifact.name}")
                return

            if result.status in RETRYABLE_STATUSES | {422}:
                matching = [
                    asset
                    for asset in self.list_assets(release_id)
                    if asset["name"] == artifact.name
                ]
                if any(
                    asset.get("state") == "uploaded"
                    and int(asset.get("size", -1)) == len(content)
                    for asset in matching
                ):
                    print(f"uploaded after HTTP {result.status}: {artifact.name}")
                    return
                for asset in matching:
                    self.delete_asset(int(asset["id"]))
                if attempt < self.client.max_attempts:
                    self.client.sleep(attempt * 3)
                    continue

            self._require(result, {201}, f"upload of {artifact.name}")
        raise AssertionError("unreachable")

    def publish_release(self, release_id: int) -> None:
        url = f"{self.releases_url}/{release_id}"
        result = self.client.request("PATCH", url, body=b'{"draft":false}')
        self._require(result, {200}, "draft release publication")

    def publish(
        self, artifacts: Path, *, title: str, notes: str, prerelease: bool
    ) -> None:
        release = self.lookup_release()
        if release is None:
            release = self.create_draft_release(title, notes, prerelease)
        should_publish = bool(release.get("draft"))

        files = sorted(path for path in artifacts.iterdir() if path.is_file())
        if not files:
            raise PublishError(f"no release artifacts found in {artifacts}")
        for artifact in files:
            self.upload_asset(release, artifact)

        if should_publish:
            self.publish_release(int(release["id"]))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--title", required=True)
    parser.add_argument("--notes-file", required=True, type=Path)
    parser.add_argument("--prerelease", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    token = os.environ.get("GH_TOKEN")
    if not token:
        raise PublishError("GH_TOKEN is required")
    publisher = ReleasePublisher(
        GitHubClient(token),
        args.repository,
        args.tag,
        api_url=os.environ.get("GITHUB_API_URL", "https://api.github.com"),
    )
    publisher.publish(
        args.artifacts,
        title=args.title,
        notes=args.notes_file.read_text(),
        prerelease=args.prerelease,
    )
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except PublishError as error:
        print(error, file=sys.stderr)
        sys.exit(1)
