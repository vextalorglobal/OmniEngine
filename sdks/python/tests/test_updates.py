"""Release-check tests.

Standard library only, and no network: every manifest is either passed
directly to :func:`omni.check_updates` or written to a temporary file and read
through a ``file://`` URL. The one test that exercises a real fetch uses the
local filesystem, which is the same code path a self-hosted mirror would take.

    python -m unittest discover -s sdks/python/tests
"""

from __future__ import annotations

import json
import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

SDK_ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(SDK_ROOT) not in sys.path:
    sys.path.insert(0, str(SDK_ROOT))

import omni  # noqa: E402  (path bootstrap must run first)


def manifest(version: str, *, abi: int | None = None, notes: str | None = None) -> str:
    document: dict[str, object] = {
        "version": version,
        "abi": omni.OMNI_ABI_VERSION if abi is None else abi,
    }
    if notes is not None:
        document["notes"] = notes
    key = omni.platform_key()
    if key is not None:
        document["assets"] = {
            key: {
                "url": "https://example.invalid/omni",
                "sha256": "a" * 64,
            }
        }
    return json.dumps(document)


def file_url(path: pathlib.Path) -> str:
    return "file:///" + str(path).replace("\\", "/").lstrip("/")


class CheckUpdate(unittest.TestCase):
    """The comparison itself, driven by known manifests."""

    def test_the_current_version_is_up_to_date(self) -> None:
        check = omni.check_updates(manifest=manifest(omni.__version__))
        self.assertEqual(check.status, "up-to-date")
        self.assertFalse(check.available)
        self.assertTrue(check.ok)
        self.assertEqual(check.latest, omni.__version__)
        self.assertEqual(check.abi, omni.OMNI_ABI_VERSION)
        self.assertIsNone(check.url)

    def test_a_newer_version_is_available_with_its_platform_asset(self) -> None:
        check = omni.check_updates(manifest=manifest("99.0.0", notes="faster lexer"))
        self.assertEqual(check.status, "available")
        self.assertTrue(check.available)
        self.assertEqual(check.latest, "99.0.0")
        self.assertEqual(check.notes, "faster lexer")
        if omni.platform_key() is not None:
            self.assertEqual(check.asset["sha256"], "a" * 64)
            self.assertEqual(check.asset["url"], "https://example.invalid/omni")
        else:
            self.assertIsNone(check.asset)
        self.assertIn("99.0.0", str(check))

    def test_an_older_release_is_not_an_upgrade(self) -> None:
        # Semver ordering, not string comparison: 0.0.1 must not look newer than
        # 0.1.0 just because it was written later.
        check = omni.check_updates(manifest=manifest("0.0.1"))
        self.assertEqual(check.status, "up-to-date")

    def test_a_different_abi_is_reported_before_any_version_comparison(self) -> None:
        check = omni.check_updates(
            manifest=manifest("99.0.0", abi=omni.OMNI_ABI_VERSION + 1)
        )
        self.assertEqual(check.status, "abi-mismatch")
        self.assertFalse(check.available)
        self.assertTrue(check.ok)
        self.assertIn("ABI", str(check))

    def test_an_unreadable_manifest_is_a_verdict_not_an_exception(self) -> None:
        check = omni.check_updates(manifest="{not json")
        self.assertEqual(check.status, "invalid")
        self.assertFalse(check.available)
        self.assertFalse(check.ok)
        self.assertIsNone(check.error)

    def test_an_oversized_manifest_is_a_bad_argument(self) -> None:
        check = omni.check_updates(manifest=" " * ((1 << 20) + 1))
        self.assertEqual(check.status, "bad-argument")


class Fetching(unittest.TestCase):
    """Transport behaviour, without a server."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = pathlib.Path(self.directory.name) / "omni-update.json"
        self.path.write_text(manifest(omni.__version__), encoding="utf-8")

    def test_a_file_url_is_read_through_the_default_fetcher(self) -> None:
        check = omni.check_updates(url=file_url(self.path))
        self.assertEqual(check.status, "up-to-date")
        self.assertEqual(check.url, file_url(self.path))

    def test_plain_http_is_refused_by_default(self) -> None:
        check = omni.check_updates(url="http://127.0.0.1:1/omni-update.json", timeout=0.1)
        self.assertEqual(check.status, "error")
        self.assertIn("plain HTTP", check.error or "")

    def test_a_transport_failure_is_returned_not_raised(self) -> None:
        def broken(_url: str, _timeout: float) -> bytes:
            raise OSError("the network is a lie")

        check = omni.check_updates(fetch=broken)
        self.assertEqual(check.status, "error")
        self.assertIn("the network is a lie", check.error or "")
        self.assertIn("update check failed", str(check))

    def test_a_transport_failure_can_be_raised_instead(self) -> None:
        def broken(_url: str, _timeout: float) -> bytes:
            raise OSError("no route to host")

        with self.assertRaises(OSError):
            omni.check_updates(fetch=broken, raise_on_error=True)

    def test_an_injected_reader_is_used_and_its_url_recorded(self) -> None:
        seen: list[tuple[str, float]] = []

        def reader(url: str, timeout: float) -> bytes:
            seen.append((url, timeout))
            return manifest("42.0.0").encode("utf-8")

        check = omni.check_updates(url="https://mirror.invalid/omni.json", fetch=reader)
        self.assertEqual(check.status, "available")
        self.assertEqual(check.url, "https://mirror.invalid/omni.json")
        self.assertEqual(seen, [("https://mirror.invalid/omni.json", 5.0)])

    def test_the_environment_names_a_source(self) -> None:
        with mock.patch.dict(os.environ, {omni.UPDATE_URL_ENV: file_url(self.path)}):
            check = omni.check_updates()
        self.assertEqual(check.status, "up-to-date")
        self.assertEqual(check.url, file_url(self.path))


class Background(unittest.TestCase):
    """The non-blocking form, and the Engine integration."""

    def test_a_background_check_finishes_with_a_result(self) -> None:
        pending = omni.start_background_check(manifest=manifest("99.0.0"))
        result = pending.wait(5.0)
        self.assertIsNotNone(result)
        self.assertTrue(pending.done)
        self.assertEqual(result.status, "available")

    def test_an_engine_can_opt_in_without_blocking(self) -> None:
        with mock.patch.object(omni, "start_background_check") as start:
            sentinel = object()
            start.return_value = sentinel
            engine = omni.Engine(background_check=True)
            self.addCleanup(engine.close)
            self.assertIs(engine.background_update, sentinel)
            start.assert_called_once_with()

    def test_an_engine_opts_out_by_default(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop(omni.UPDATE_CHECK_ENV, None)
            engine = omni.Engine()
            self.addCleanup(engine.close)
            self.assertIsNone(engine.background_update)

    def test_the_environment_turns_the_background_check_on(self) -> None:
        with mock.patch.dict(os.environ, {omni.UPDATE_CHECK_ENV: "1"}):
            with mock.patch.object(omni, "start_background_check") as start:
                start.return_value = object()
                engine = omni.Engine()
                self.addCleanup(engine.close)
                start.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()
