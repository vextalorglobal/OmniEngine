"""Permission tests for the Python SDK: the ABI 5 grant surface.

Standard library only, so they run with no install step:

    python sdks/python/tests/test_permissions.py

They need the built engine: `cargo build -p omni-ffi`. The rules under test
are the sandbox contract: no grant means every filesystem touch fails closed
with a positioned error, a grant opens exactly the path it names, sibling
engines never inherit anything, and parallel workers carry the parent's roots.
"""

from __future__ import annotations

import pathlib
import sys
import tempfile
import unittest

SDK_ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(SDK_ROOT) not in sys.path:
    sys.path.insert(0, str(SDK_ROOT))

import omni  # noqa: E402  (path bootstrap must run first)


def _quoted(path: pathlib.Path | str) -> str:
    """A path as an Omni string literal, backslashes escaped for the lexer."""
    escaped = str(path).replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


class PermissionSandbox(unittest.TestCase):
    """The fail-closed default and what a grant opens."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)

    def test_reads_fail_closed_until_the_root_is_granted(self) -> None:
        target = self.root / "a.txt"
        target.write_text("from a", encoding="utf-8")

        with self.assertRaises(omni.OmniError) as denied:
            self.engine.run(f"fs.read_text({_quoted(target)})")
        self.assertEqual(denied.exception.kind, "runtime")
        self.assertIn(
            "fs.read_text is disabled: this engine has no filesystem read permission",
            denied.exception.message,
        )

        self.engine.allow_fs_read(self.root)
        result = self.engine.run(f"fs.read_text({_quoted(target)})")
        self.assertEqual(result.value, "from a")

    def test_a_grant_opens_one_root_and_names_the_request_when_denied(self) -> None:
        outside_dir = tempfile.TemporaryDirectory()
        self.addCleanup(outside_dir.cleanup)
        inside = self.root / "in.txt"
        outside = pathlib.Path(outside_dir.name) / "out.txt"
        inside.write_text("inside", encoding="utf-8")
        outside.write_text("outside", encoding="utf-8")

        self.engine.allow_fs_read(self.root)
        self.assertEqual(
            self.engine.run(f"fs.read_text({_quoted(inside)})").value, "inside"
        )

        with self.assertRaises(omni.OmniError) as denied:
            self.engine.run(f"fs.read_text({_quoted(outside)})")
        self.assertEqual(denied.exception.kind, "runtime")
        self.assertIn(
            f"path '{outside}' is outside the allowed read roots",
            denied.exception.message,
        )

    def test_writes_fail_closed_until_the_root_is_granted(self) -> None:
        target = self.root / "a.txt"

        with self.assertRaises(omni.OmniError) as denied:
            self.engine.run(f"fs.write_text({_quoted(target)}, \"x\")")
        self.assertEqual(denied.exception.kind, "runtime")
        self.assertIn(
            "fs.write_text is disabled: this engine has no filesystem write permission",
            denied.exception.message,
        )
        self.assertFalse(target.exists(), "a denied write must not touch the disk")

        self.engine.allow_fs_write(self.root)
        self.engine.run(f"fs.write_text({_quoted(target)}, \"written\")")
        self.assertEqual(target.read_text(encoding="utf-8"), "written")

    def test_the_module_root_gates_imports_until_granted(self) -> None:
        (self.root / "helper.omni").write_text("let answer = 40 + 2", encoding="utf-8")

        with self.assertRaises(omni.OmniError) as denied:
            self.engine.run('import "helper"\nhelper.answer')
        self.assertEqual(denied.exception.kind, "runtime")
        self.assertIn(
            "import is disabled: this engine has no module root",
            denied.exception.message,
        )

        self.engine.set_module_root(self.root)
        self.assertEqual(
            self.engine.run('import "helper"\nhelper.answer').value, 42.0
        )

    def test_grants_never_leak_between_engines(self) -> None:
        target = self.root / "n.txt"
        target.write_text("7", encoding="utf-8")

        sibling = omni.Engine()
        self.addCleanup(sibling.close)
        self.engine.allow_fs_read(self.root)
        self.assertEqual(
            self.engine.run(f"fs.read_text({_quoted(target)})").value, "7"
        )

        with self.assertRaises(omni.OmniError) as denied:
            sibling.run(f"fs.read_text({_quoted(target)})")
        self.assertIn(
            "fs.read_text is disabled: this engine has no filesystem read permission",
            denied.exception.message,
        )

    def test_parallel_workers_inherit_the_parents_grants(self) -> None:
        target = self.root / "n.txt"
        target.write_text("7", encoding="utf-8")
        program = f"fs.read_text({_quoted(target)})"

        self.engine.allow_fs_read(self.root)
        results = self.engine.run_parallel([program, program], max_threads=2)
        self.assertEqual([r.value for r in results], ["7", "7"])

    def test_host_misuse_is_an_argument_error_not_a_crash(self) -> None:
        for grant in (
            self.engine.allow_fs_read,
            self.engine.allow_fs_write,
            self.engine.set_module_root,
        ):
            with self.assertRaises(omni.OmniError) as empty:
                grant("")
            self.assertEqual(empty.exception.kind, "argument")

            with self.assertRaises(omni.OmniError) as missing:
                grant(self.root / "no-such-directory")
            self.assertEqual(missing.exception.kind, "argument")

            with self.assertRaises(omni.OmniError) as overlong:
                grant("a" * (4096 + 1))
            self.assertEqual(overlong.exception.kind, "argument")

        # Refused grants change nothing: the engine still evaluates.
        self.assertEqual(self.engine.evaluate("1 + 1"), 2.0)

    def test_pathlike_arguments_are_accepted(self) -> None:
        target = self.root / "p.txt"
        target.write_text("ok", encoding="utf-8")
        self.engine.allow_fs_read(self.root / ".")  # a pathlib.Path, not a str
        self.assertEqual(self.engine.run(f"fs.read_text({_quoted(target)})").value, "ok")


if __name__ == "__main__":
    unittest.main()
