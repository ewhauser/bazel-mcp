#!/usr/bin/env python3

import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.error import HTTPError

from scripts.publish_github_release_assets import (
    GitHubClient,
    HttpResult,
    ReleasePublisher,
)


def release(*, draft: bool = False) -> dict[str, object]:
    return {
        "id": 42,
        "draft": draft,
        "upload_url": "https://uploads.github.com/repos/o/r/releases/42/assets{?name,label}",
    }


class ReleasePublisherTests(unittest.TestCase):
    def artifact_dir(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        (root / "artifact.tar.xz").write_bytes(b"artifact")
        return directory, root

    def test_uploads_to_existing_release(self) -> None:
        directory, root = self.artifact_dir()
        self.addCleanup(directory.cleanup)
        client = Mock(max_attempts=5, sleep=Mock())
        client.request.side_effect = [
            HttpResult(200, json.dumps(release()).encode()),
            HttpResult(200, b"[]"),
        ]
        client.request_once.return_value = HttpResult(201, b"{}")

        ReleasePublisher(client, "o/r", "v1").publish(
            root, title="v1", notes="notes", prerelease=False
        )

        self.assertEqual(client.request_once.call_count, 1)

    def test_accepts_ambiguous_upload_when_asset_is_present(self) -> None:
        directory, root = self.artifact_dir()
        self.addCleanup(directory.cleanup)
        client = Mock(max_attempts=5, sleep=Mock())
        client.request.side_effect = [
            HttpResult(200, json.dumps(release()).encode()),
            HttpResult(200, b"[]"),
            HttpResult(
                200,
                b'[{"id":7,"name":"artifact.tar.xz","state":"uploaded","size":8}]',
            ),
        ]
        client.request_once.return_value = HttpResult(503, b"unavailable")

        ReleasePublisher(client, "o/r", "v1").publish(
            root, title="v1", notes="notes", prerelease=False
        )

        client.sleep.assert_not_called()

    def test_creates_draft_then_publishes_after_upload(self) -> None:
        directory, root = self.artifact_dir()
        self.addCleanup(directory.cleanup)
        client = Mock(max_attempts=5, sleep=Mock())
        client.request.side_effect = [
            HttpResult(404, b"{}"),
            HttpResult(200, b"[]"),
            HttpResult(200, b"{}"),
        ]
        client.request_once.side_effect = [
            HttpResult(201, json.dumps(release(draft=True)).encode()),
            HttpResult(201, b"{}"),
        ]

        ReleasePublisher(client, "o/r", "v1").publish(
            root, title="v1", notes="notes", prerelease=False
        )

        patch_call = client.request.call_args_list[-1]
        self.assertEqual(patch_call.args[0], "PATCH")

    @patch("scripts.publish_github_release_assets.urlopen")
    def test_client_retries_http_503(self, mocked_urlopen: Mock) -> None:
        unavailable = HTTPError(
            "https://api.github.test/release",
            503,
            "unavailable",
            {},
            io.BytesIO(b"unavailable"),
        )
        response = Mock(status=200)
        response.read.return_value = b"{}"
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        mocked_urlopen.side_effect = [unavailable, response]
        sleep = Mock()

        result = GitHubClient("token", sleep=sleep).request(
            "GET", "https://api.github.test/release"
        )

        self.assertEqual(result.status, 200)
        sleep.assert_called_once_with(3)


if __name__ == "__main__":
    unittest.main()
