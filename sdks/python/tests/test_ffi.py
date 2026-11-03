"""Python SDK tests.

Standard library only, so they run with no install step:

    python sdks/python/tests/test_ffi.py
    python -m unittest discover -s sdks/python/tests

They need the built engine: `cargo build -p omni-ffi`.
"""

from __future__ import annotations

import pathlib
import sys
import unittest

SDK_ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(SDK_ROOT) not in sys.path:
    sys.path.insert(0, str(SDK_ROOT))

import omni  # noqa: E402  (path bootstrap must run first)
from omni import _ffi  # noqa: E402


class VerticalSlice(unittest.TestCase):
    """Acceptance criterion: 42 out of Python, unchanged."""

    def test_forty_two(self) -> None:
        with omni.Engine() as engine:
            self.assertEqual(engine.evaluate("let answer = 40 + 2"), 42.0)

    def test_forty_two_reports_as_a_number_tag(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        result = engine.run("let answer = 40 + 2")
        self.assertEqual(result.tag, "number")
        self.assertEqual(result.value, 42.0)
        self.assertEqual(str(result), "42.0")

    def test_library_version_is_reported(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        self.assertRegex(engine.version, r"^\d+\.\d+\.\d+")

    def test_arithmetic_precedence_and_parentheses(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        self.assertEqual(engine.evaluate("2 + 3 * 4"), 14.0)
        self.assertEqual(engine.evaluate("(2 + 3) * 4"), 20.0)
        self.assertEqual(engine.evaluate("20 % 7"), 6.0)
        self.assertAlmostEqual(engine.evaluate("20 / 3"), 20 / 3)
        self.assertEqual(engine.evaluate("-3 + 10"), 7.0)

    def test_repeated_calls_reuse_one_engine(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        for _ in range(50):
            self.assertEqual(engine.evaluate("1 + 1"), 2.0)

    def test_two_engines_do_not_share_bindings(self) -> None:
        a = omni.Engine()
        b = omni.Engine()
        self.addCleanup(a.close)
        self.addCleanup(b.close)
        a.run("let x = 1")
        with self.assertRaises(omni.OmniError) as caught:
            b.run("x")
        self.assertEqual(caught.exception.kind, "runtime")


class Errors(unittest.TestCase):
    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_syntax_error_is_a_compile_error_with_text(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("let = 1")
        self.assertEqual(caught.exception.status, 1)
        self.assertEqual(caught.exception.kind, "compile")
        self.assertIn("line", caught.exception.message)

    def test_division_by_zero_is_a_runtime_error(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.evaluate("1 / 0")
        self.assertEqual(caught.exception.kind, "runtime")
        # The line number crosses the ABI boundary, so a host can point at source.
        self.assertEqual(caught.exception.message, "line 1: division by zero")

    def test_evaluate_refuses_a_non_numeric_program(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.evaluate("nope")
        self.assertEqual(caught.exception.kind, "runtime")
        # exec_number's type failure is raised by the ABI layer, not the engine,
        # so the wrapper has to supply the message rather than return silence.
        self.assertTrue(caught.exception.message)

    def test_the_line_number_points_at_the_failing_line(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("// a comment\nlet = 1")
        self.assertTrue(caught.exception.message.startswith("line 2: "), caught.exception.message)

    def test_an_empty_program_is_undef_not_an_error(self) -> None:
        self.assertEqual(self.engine.run("").tag, "undef")

    def test_a_wrapper_rejected_type_supplies_its_own_message(self) -> None:
        # An empty program succeeds in the engine and yields undef, so
        # evaluate()'s refusal comes from the ABI layer -- where the engine has
        # no error text -- and the wrapper must not fail silently.
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.evaluate("")
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertIn("did not evaluate to a number", caught.exception.message)

    def test_try_run_returns_status_instead_of_raising(self) -> None:
        status, result, message = self.engine.try_run("let = 1")
        self.assertEqual(status, 1)
        self.assertIsNone(result)
        self.assertTrue(message)
        self.assertEqual(self.engine.last_status, 1)

    def test_a_success_clears_the_stale_error(self) -> None:
        with self.assertRaises(omni.OmniError):
            self.engine.run("###")
        self.assertTrue(self.engine.last_error)
        self.engine.run("1")
        self.assertEqual(self.engine.last_error, "")


class SafetyBoundaries(unittest.TestCase):
    """The cases a host is most likely to get wrong, tested on purpose."""

    def test_a_panicking_engine_returns_a_status_and_survives(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        # Rust prints the panic to stderr; the contract is that the process does
        # not die, which is what the assertion below proves.
        self.assertEqual(raw.debug_panic(), 3)
        status, value = raw.exec("1 + 1")
        self.assertEqual(status, 0)
        self.assertEqual(value.number, 2.0)

    def test_error_text_is_copied_out_and_truncates(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        raw.exec("1 / 0")  # "line 1: division by zero"
        self.assertEqual(raw.last_error(cap=100), "line 1: division by zero")
        # The ABI writes at most cap - 1 bytes so there is always room for a NUL.
        self.assertEqual(raw.last_error(cap=8), "line 1:")
        self.assertEqual(raw.last_error(cap=1), "")
        self.assertEqual(raw.last_error(cap=0), "")

    def test_source_larger_than_the_abi_allows_is_rejected_locally(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        original = _ffi.MAX_SOURCE_LEN
        _ffi.MAX_SOURCE_LEN = 16
        try:
            with self.assertRaises(omni.OmniError) as caught:
                raw.exec("1" * 17)
            self.assertEqual(caught.exception.kind, "argument")
        finally:
            _ffi.MAX_SOURCE_LEN = original

    def test_very_long_real_source_still_works(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        program = "1" + " + 1" * 2000
        self.assertEqual(engine.evaluate(program), 2001.0)

    def test_non_ascii_source_is_accepted(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        # A comment in Bengali, then the answer. UTF-8 in, bytes out to the ABI.
        self.assertEqual(engine.evaluate("// নম্বর\n42"), 42.0)

    def test_closed_engine_refuses_work_and_double_close_is_safe(self) -> None:
        engine = omni.Engine()
        engine.close()
        engine.close()
        with self.assertRaises(omni.OmniError) as caught:
            engine.run("1")
        self.assertEqual(caught.exception.kind, "argument")
        self.assertEqual(repr(engine), "<omni.Engine closed>")


class Discovery(unittest.TestCase):
    def test_candidates_prefer_an_explicit_library(self) -> None:
        home, library = _ffi.os.environ.get("OMNI_ENGINE_HOME"), _ffi.os.environ.get("OMNI_ENGINE_LIBRARY")
        _ffi.os.environ["OMNI_ENGINE_LIBRARY"] = "/tmp/somewhere/omni.dll"
        _ffi.os.environ["OMNI_ENGINE_HOME"] = "/tmp/omnihome"
        try:
            paths = _ffi._candidate_paths()
            self.assertEqual(paths[0], "/tmp/somewhere/omni.dll")
            self.assertIn(_ffi.os.path.join("/tmp/omnihome", "target", "release", _ffi.library_file_name()), paths)
        finally:
            for key, value in (("OMNI_ENGINE_HOME", home), ("OMNI_ENGINE_LIBRARY", library)):
                if value is None:
                    _ffi.os.environ.pop(key, None)
                else:
                    _ffi.os.environ[key] = value

    def test_workspace_target_directory_is_searched(self) -> None:
        paths = _ffi._candidate_paths()
        name = _ffi.library_file_name()
        self.assertTrue(
            any(p.endswith(name) and f"{_ffi.os.sep}target{_ffi.os.sep}debug" in p for p in paths),
            f"expected a workspace target/debug search among {paths}",
        )

    def test_the_directory_you_pass_in_counts_not_only_its_parents(self) -> None:
        # Running from the repository root is the case that must work with no
        # environment setup, so the walk starts at the base rather than above it.
        name = _ffi.library_file_name()
        base = _ffi.os.path.abspath("/some/project")  # abspath, so drive letters match
        paths = _ffi._upward("/some/project", name)
        self.assertIn(_ffi.os.path.join(base, "target", "debug", name), paths)
        self.assertIn(_ffi.os.path.join(base, "target", "release", name), paths)
        self.assertTrue(any("target" in p for p in paths), paths)

    def test_library_file_name_matches_the_platform(self) -> None:
        name = _ffi.library_file_name()
        if sys.platform == "win32":
            self.assertEqual(name, "omni.dll")
        elif sys.platform == "darwin":
            self.assertEqual(name, "libomni.dylib")
        else:
            self.assertEqual(name, "libomni.so")

    def test_the_built_library_is_reachable(self) -> None:
        lib = _ffi.load_library()
        self.assertTrue(lib.omni_version())

    def test_a_missing_library_names_the_build_command(self) -> None:
        home = _ffi.os.environ.get("OMNI_ENGINE_HOME")
        library = _ffi.os.environ.get("OMNI_ENGINE_LIBRARY")
        real_name = _ffi.library_file_name
        _ffi.os.environ.pop("OMNI_ENGINE_HOME", None)
        _ffi.os.environ.pop("OMNI_ENGINE_LIBRARY", None)
        # A file name nothing can have on disk, so neither the search order nor
        # the loader's own PATH lookup can accidentally succeed.
        _ffi.library_file_name = lambda: "omni-definitely-not-here.dll"  # type: ignore[assignment]
        try:
            with self.assertRaises(omni.OmniLibraryError) as caught:
                _ffi.load_library()
            self.assertIn("cargo build -p omni-ffi", str(caught.exception))
        finally:
            _ffi.library_file_name = real_name  # type: ignore[assignment]
            if home is not None:
                _ffi.os.environ["OMNI_ENGINE_HOME"] = home
            if library is not None:
                _ffi.os.environ["OMNI_ENGINE_LIBRARY"] = library


class AbiLayout(unittest.TestCase):
    def test_value_struct_matches_the_c_header(self) -> None:
        self.assertEqual(_ffi.ctypes.sizeof(_ffi.OmniValue), 16)
        self.assertEqual(_ffi.OmniValue.number.offset, 8)
        self.assertEqual(_ffi.OmniValue.tag.offset, 0)

    def test_status_and_tag_names(self) -> None:
        self.assertEqual(omni.status_name(0), "ok")
        self.assertEqual(omni.status_name(4), "argument")
        self.assertEqual(omni.status_name(99), "status-99")
        self.assertEqual(omni.tag_name(0), "number")
        self.assertEqual(omni.tag_name(3), "undef")

    def test_bool_and_undef_values_convert(self) -> None:
        # Phase 1 has no literals for these, so build the struct directly.
        raw = _ffi.OmniValue()
        raw.tag, raw.number = _ffi.OMNI_BOOL, 1.0
        self.assertEqual(omni._convert(raw).value, True)
        raw.tag, raw.number = _ffi.OMNI_UNDEF, 0.0
        self.assertIsNone(omni._convert(raw).value)


if __name__ == "__main__":
    unittest.main(verbosity=2)
