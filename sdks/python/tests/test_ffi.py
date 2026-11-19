"""Python SDK tests.

Standard library only, so they run with no install step:

    python sdks/python/tests/test_ffi.py
    python -m unittest discover -s sdks/python/tests

They need the built engine: `cargo build -p omni-ffi`. Note the *build*, not
`cargo test`: the tests load the artifact on disk, and `cargo test -p omni-ffi`
does not refresh it.
"""

from __future__ import annotations

import pathlib
import struct
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

    def test_library_version_and_abi_level_are_reported(self) -> None:
        engine = omni.Engine()
        self.addCleanup(engine.close)
        self.assertRegex(engine.version, r"^\d+\.\d+\.\d+")
        # The SDK refused to hand back a library whose layout this file cannot
        # read, so by the time an Engine exists the level is known.
        self.assertEqual(engine.abi_version, _ffi.OMNI_ABI_VERSION)
        self.assertEqual(engine.abi_version, 5)

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


class Strings(unittest.TestCase):
    """Phase 2's new payload: text crosses the boundary as copied bytes."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_concatenation_comes_back_as_a_string(self) -> None:
        result = self.engine.run(r'"omni" + " " + "engine"')
        self.assertEqual(result.tag, "string")
        self.assertEqual(result.value, "omni engine")
        self.assertEqual(result.text, b"omni engine")
        self.assertEqual(str(result), "'omni engine'")

    def test_a_string_result_reports_no_number(self) -> None:
        # The eight payload bytes of a string are an address. Reporting them as
        # a double would put meaningless noise in every log that prints a
        # Result, so the wrapper reads that arm only for the number tags.
        result = self.engine.run('"x"')
        self.assertEqual(result.number, 0.0)
        self.assertEqual(result.tag, "string")

    def test_escapes_survive_the_boundary_byte_for_byte(self) -> None:
        # Raw strings on the left: the source really does contain a backslash,
        # and the engine is what turns it into the character.
        cases = {
            r'"a\nb"': b"a\nb",
            r'"a\tb"': b"a\tb",
            r'"a\rb"': b"a\rb",
            r'"a\"b"': b'a"b',
            r'"a\\b"': b"a\\b",
            r'"\u{41}\u{42}"': b"AB",
            r'"\u{1F600}"': "\U0001f600".encode(),
        }
        for source, expected in cases.items():
            with self.subTest(source=source):
                result = self.engine.run(source)
                self.assertEqual(result.tag, "string")
                self.assertEqual(result.text, expected)

    def test_a_string_may_contain_a_nul_byte(self) -> None:
        # Length-delimited, not C strings: `len` is the only end-of-string, so
        # text after a NUL must still arrive.
        result = self.engine.run(r'"a" + "\u{0}" + "b"')
        self.assertEqual(result.text, b"a\x00b")
        self.assertEqual(len(result.value), 3)
        self.assertEqual(result.value, "a\x00b")

    def test_a_multiline_string_is_a_compile_error_not_a_truncated_result(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run('"a\nb"')  # a real newline inside the literal
        self.assertEqual(caught.exception.kind, "compile")
        self.assertIn("cannot span lines", caught.exception.message)

    def test_non_ascii_text_keeps_its_bytes(self) -> None:
        # Bengali in the source, Bengali out: the ABI carries UTF-8 bytes, and
        # `text` counts those while `value` counts characters.
        expected = "নমস্কার!"
        result = self.engine.run(r'"নমস্কার" + "!"')
        self.assertEqual(result.value, expected)
        self.assertEqual(result.text, expected.encode("utf-8"))
        self.assertEqual(len(result.text), 22)  # seven 3-byte code units plus "!"
        self.assertEqual(len(result.value), 8)
        self.assertEqual(result.text.decode("utf-8"), result.value)

    def test_equality_compares_content_not_addresses(self) -> None:
        self.assertTrue(self.engine.run(r'"ab" == "a" + "b"').value)
        self.assertFalse(self.engine.run(r'"ab" == "ac"').value)
        self.assertTrue(self.engine.run(r'"ab" != "a"').value)

    def test_the_empty_string_is_a_string_not_undef(self) -> None:
        result = self.engine.run(r'""')
        self.assertEqual(result.tag, "string")
        self.assertEqual(result.value, "")
        self.assertEqual(result.text, b"")
        self.assertEqual(self.engine.run("").tag, "undef")

    def test_mixing_a_string_and_a_number_names_the_rule(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run(r'"hi" + 42')
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertEqual(caught.exception.message, r'[Line 1, Col 6]: type mismatch: cannot add string to number: "hi" + 42')

    def test_evaluate_refuses_a_string_and_string_refuses_a_number(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.evaluate('"text"')
        self.assertEqual(caught.exception.kind, "runtime")
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.string("1 + 1")
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertIn("did not evaluate to a string", caught.exception.message)
        self.assertEqual(self.engine.string(r'"fine"'), "fine")

    def test_a_copied_string_is_unaffected_by_the_next_execution(self) -> None:
        # The egress block rewinds on the next call. `result` was built before
        # that call and is still correct afterwards, because it holds Python
        # bytes rather than an address.
        result = self.engine.run('"kept"')
        for _ in range(20):
            self.engine.run('"overwritten with other text entirely"')
        self.assertEqual(result.value, "kept")


class StaleStrings(unittest.TestCase):
    """What a host that ignored the copy rule would have seen -- tested instead."""

    def setUp(self) -> None:
        self.raw = _ffi.RawEngine()
        self.addCleanup(self.raw.close)

    def test_a_stale_value_reads_the_bytes_that_are_there_now(self) -> None:
        _, first = self.raw.exec('"first"')
        self.assertEqual(self.raw.read_string(first), b"first")
        self.raw.exec('"a much longer second"')
        # Not a segfault and not the old text: the address still points into
        # live, engine-owned memory, and the copy is bounded by the stale value's
        # own `len` hint. This is the documented consequence of a rewind.
        self.assertEqual(self.raw.read_string(first), b"a muc")

    def test_a_foreign_or_fabricated_address_copies_nothing(self) -> None:
        _, value = self.raw.exec('"real"')
        value.ptr = 0x7FFF_EFFF_FFFF_FFF0  # inside 48 bits, owned by nothing
        self.assertEqual(self.raw.read_string(value), b"")
        value.ptr = 1 << 50  # outside the address bits the value format carries
        self.assertEqual(self.raw.read_string(value), b"")

    def test_read_string_ignores_a_value_that_is_not_a_string(self) -> None:
        _, value = self.raw.exec("1 + 1")
        self.assertEqual(value.tag, _ffi.OMNI_NUMBER)
        self.assertEqual(self.raw.read_string(value), b"")


class HostStrings(unittest.TestCase):
    """`define_string`: bytes the host owns, bound for the engine's lifetime."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_a_host_string_is_readable_by_the_program(self) -> None:
        self.engine.define_string("greeting", "hello wörld")
        result = self.engine.run('greeting + "!"')
        self.assertEqual(result.value, "hello wörld!")
        # 12 bytes of UTF-8 for the payload, 13 with the bang.
        self.assertEqual(len(result.text), 13)

    def test_a_host_string_survives_executions_a_result_does_not(self) -> None:
        self.engine.define_string("stable", "still here")
        for _ in range(50):
            self.engine.run('"unrelated work"')
        self.assertEqual(self.engine.run("stable").value, "still here")

    def test_redefining_replaces_the_binding(self) -> None:
        self.engine.define_string("name", "before")
        self.engine.define_string("name", "after")
        self.assertEqual(self.engine.run("name").value, "after")

    def test_an_old_value_of_a_redefined_name_copies_nothing(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        self.assertEqual(raw.define_string("name", "before"), _ffi.OMNI_OK)
        status, value = raw.exec("name")
        self.assertEqual(status, _ffi.OMNI_OK)
        self.assertEqual(raw.read_string(value), b"before")
        raw.define_string("name", "a longer replacement")
        # The previous binding was released, and the engine no longer
        # recognises the address: zero bytes, not freed memory.
        self.assertEqual(raw.read_string(value), b"")
        self.assertEqual(raw.read_string(raw.exec("name")[1]), b"a longer replacement")

    def test_the_name_is_validated_before_the_call(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.define_string("", "value")
        self.assertEqual(caught.exception.kind, "argument")

    def test_a_name_too_long_is_rejected(self) -> None:
        original = _ffi.MAX_IDENTIFIER_LEN
        _ffi.MAX_IDENTIFIER_LEN = 8
        try:
            with self.assertRaises(omni.OmniError) as caught:
                self.engine.define_string("n" * 9, "value")
            self.assertEqual(caught.exception.kind, "argument")
            self.assertIn("at most 8", caught.exception.message)
        finally:
            _ffi.MAX_IDENTIFIER_LEN = original
        # And the ABI's own ceiling is the same number, so a name that passes
        # here is a name the library will accept.
        self.assertEqual(_ffi.MAX_IDENTIFIER_LEN, 32)

    def test_a_non_utf8_payload_is_rejected_by_the_library(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        status = raw.define_string("bad", b"\xff\xfe")
        self.assertEqual(status, _ffi.OMNI_ERR_ARG)
        self.assertEqual(raw.last_error(), "")  # the status is the whole report


class HostFunctions(unittest.TestCase):
    """Phase 3's new direction: a script calls *out* into Python.

    Every test here is also a lifetime test. A registration hands C the address
    of a ctypes closure, so the assertions keep coming back to the same two
    questions -- is the closure still alive when the engine calls it, and is it
    released only once the engine has forgotten it?
    """

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)
        self.calls: list[object] = []

    def spy(self, result: object = "spied"):
        """A callable that records what it was given and answers with `result`."""

        def fn(value):
            self.calls.append(value)
            return result

        return fn

    def test_a_python_function_answers_a_script(self) -> None:
        # The acceptance criterion, stated in the spec: a host-defined function
        # called from an .omni program returns the value it computed.
        self.engine.register_function("square", lambda x: x * x, arity=1)
        self.assertEqual(self.engine.evaluate("square(6) + 1"), 37.0)
        self.assertEqual(self.engine.host_function_count, 1)

    def test_arguments_arrive_as_python_data(self) -> None:
        self.engine.register_function("spy", self.spy(0.0), arity=1)
        self.assertEqual(self.calls, [])
        self.engine.run("spy(1.5)")
        self.assertEqual(self.calls, [1.5])
        self.calls.clear()
        self.engine.run('spy("text")')
        self.assertEqual(self.calls, ["text"])
        self.calls.clear()
        self.engine.run("spy(1 == 1)")  # the grammar has no boolean literal
        self.assertEqual(self.calls, [True])

    def test_a_string_argument_its_bytes_and_no_more(self) -> None:
        # The borrowed view is exactly `len` bytes long. A Python function that
        # reads to a NUL would stop early; one that over-reads would see the next
        # allocation. Length is the only answer, and interior NULs prove it.
        seen: list[str] = []
        self.engine.register_function("collect", lambda s: seen.append(s) or "-", arity=1)
        self.engine.run(r'collect("a" + "\u{0}" + "b")')
        self.assertEqual(seen, ["a\x00b"])

    def test_return_types_become_the_values_they_look_like(self) -> None:
        cases = {
            "num": (lambda: 42, "number", 42.0),
            "neg": (lambda: -1.5, "number", -1.5),
            "yes": (lambda: True, "bool", True),
            "no": (lambda: False, "bool", False),
            "nothing": (lambda: None, "nil", None),
            "text": (lambda: "hi", "string", "hi"),
            "raw": (lambda: b"bytes", "string", "bytes"),
            "empty": (lambda: "", "string", ""),
        }
        for name, (fn, tag, value) in cases.items():
            with self.subTest(name=name):
                self.engine.register_function(name, fn, arity=0)
                result = self.engine.run(f"{name}()")
                self.assertEqual(result.tag, tag)
                self.assertEqual(result.value, value)

    def test_a_returned_string_is_the_engines_after_the_next_execution(self) -> None:
        # The trampoline keeps its output buffer until the *next* call replaces
        # it, which is what lets the engine read it after Python has returned.
        # Result.text is Python bytes either way, so a later run cannot disturb it.
        self.engine.register_function("greet", lambda name: f"hello {name}", arity=1)
        first = self.engine.run('greet("fahim")')
        for _ in range(20):
            self.engine.run('greet("someone much longer than the first answer")')
        self.assertEqual(first.value, "hello fahim")

    def test_nesting_and_arity_are_checked_before_python_is_entered(self) -> None:
        self.engine.register_function("add", lambda a, b: a + b, arity=2)
        self.assertEqual(self.engine.evaluate("add(add(1, 2), 3)"), 6.0)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("add(1)")
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertIn("takes 2 arguments, got 1", caught.exception.message)

    def test_a_wrong_count_never_calls_the_host(self) -> None:
        self.engine.register_function("never", lambda x: x, arity=1)
        with self.assertRaises(omni.OmniError):
            self.engine.run("never(1, 2)")
        # `never` ran zero times: arity is the engine's check, not the host's.
        self.assertEqual(self.engine.evaluate("1 + 1"), 2.0)

    def test_an_unregistered_name_fails_where_it_is_called(self) -> None:
        # Name resolution happens at evaluation, not at parse: the grammar cannot
        # know which names a host will register, so an unknown callee is a
        # runtime error that says so and gives the line.
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("nothere(1)")
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertIn("unknown function 'nothere'", caught.exception.message)

    def test_more_arguments_than_the_abi_carries_is_refused(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.register_function("wide", lambda *a: 0.0, arity=_ffi.MAX_ARGS + 1)
        self.assertEqual(caught.exception.kind, "argument")
        self.assertIn("0 and 16", caught.exception.message)
        self.assertEqual(self.engine.host_function_count, 0)
        # And at the other end, a function of 16 arguments is registerable.
        self.engine.register_function("wide", lambda *a: float(len(a)), arity=_ffi.MAX_ARGS)
        args = ", ".join(str(i) for i in range(_ffi.MAX_ARGS))
        self.assertEqual(self.engine.evaluate(f"wide({args})"), float(_ffi.MAX_ARGS))

    def test_a_python_exception_becomes_a_host_error_not_a_crash(self) -> None:
        def boom(value):
            raise RuntimeError("the host said no")

        self.engine.register_function("boom", boom, arity=1)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("boom(1) + 1")
        self.assertEqual(caught.exception.kind, "host")
        self.assertEqual(caught.exception.status, _ffi.OMNI_ERR_HOST)
        self.assertIn("[Line 1, Col 1]", caught.exception.message)
        self.assertIn("host function 'boom' failed", caught.exception.message)
        self.assertIn("RuntimeError: the host said no", caught.exception.message)
        # The engine is still standing, and still knows the function.
        self.assertEqual(self.engine.evaluate("1 + 1"), 2.0)

    def test_a_return_value_python_cannot_pass_is_reported(self) -> None:
        self.engine.register_function("odd", lambda: {"not": "a value"}, arity=0)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("odd()")
        self.assertEqual(caught.exception.kind, "host")
        self.assertIn("must return a number, bool, str, bytes or None, not dict", caught.exception.message)

    def test_a_callback_that_calls_back_in_is_refused_not_corrupting(self) -> None:
        # Re-entrancy on one handle is the soundness case the ABI refuses: the
        # engine is borrowed by the call that entered Python, so Python's own
        # run() gets status 6, which the trampoline turns into a host error.
        def reenter(value):
            return self.engine.evaluate("1 + 1")

        self.engine.register_function("reenter", reenter, arity=1)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("reenter(1)")
        self.assertEqual(caught.exception.kind, "host")
        self.assertIn("already inside a host function", caught.exception.message)

    def test_unregister_lets_the_script_stop_seeing_the_name(self) -> None:
        self.engine.register_function("temp", lambda: 1.0, arity=0)
        self.assertEqual(self.engine.evaluate("temp()"), 1.0)
        self.assertTrue(self.engine.unregister_function("temp"))
        self.assertEqual(self.engine.host_function_count, 0)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("temp()")
        self.assertEqual(caught.exception.kind, "runtime")
        # Idempotent, and it answers honestly the second time.
        self.assertFalse(self.engine.unregister_function("temp"))

    def test_redefining_a_name_replaces_the_callable(self) -> None:
        self.engine.register_function("f", lambda: 1.0, arity=0)
        self.engine.register_function("f", lambda: 2.0, arity=0)
        self.assertEqual(self.engine.evaluate("f()"), 2.0)
        self.assertEqual(self.engine.host_function_count, 1)

    def test_a_non_callable_is_rejected_before_the_abi_sees_it(self) -> None:
        # Node can ask `typeof` for free, and Python can ask `callable`; both
        # decline the registration rather than storing a trampoline that fails as a
        # host error several evaluations later.
        for not_a_function in (42, "shout", None, ["a", "list"]):
            with self.assertRaises(omni.OmniError) as caught:
                self.engine.register_function("bad", not_a_function, arity=0)
            self.assertEqual(caught.exception.kind, "argument")
        self.assertEqual(self.engine.host_function_count, 0)

    def test_a_name_binding_and_a_function_of_the_same_name_coexist(self) -> None:
        # define_string owns a variable, register_function owns a call site; the
        # grammar tells them apart by the parenthesis, so both are legal.
        self.engine.define_string("pair", "two words")
        self.engine.register_function("pair", lambda: 2.0, arity=0)
        self.assertEqual(self.engine.run("pair").value, "two words")
        self.assertEqual(self.engine.evaluate("pair()"), 2.0)

    def test_a_registration_survives_interpretations_and_a_dropped_name(self) -> None:
        import gc

        def long_lived(value):
            return value

        self.engine.register_function("keep", long_lived, arity=1)
        # Drop every Python name that could be the only thing keeping the closure
        # alive, then collect. If the SDK had handed C a borrowed reference, this
        # is where the interpreter would die -- instead the engine still calls it.
        del long_lived
        gc.collect()
        for _ in range(2_000):
            self.assertEqual(self.engine.run("keep(7)").value, 7.0)
        self.assertEqual(self.engine.host_function_count, 1)

    def test_the_rentrancy_guard_leaves_memory_flat(self) -> None:
        # Registrations are not arena allocations, so 2,000 callbacks reserve
        # nothing beyond what the first one did.
        self.engine.register_function("echo", lambda s: s, arity=1)
        self.engine.run('echo("prime")')
        before = self.engine.stats()
        for _ in range(2_000):
            self.assertEqual(self.engine.run('echo("a string through python")').value, "a string through python")
        after = self.engine.stats()
        self.assertEqual(after.arena_bytes_reserved, before.arena_bytes_reserved)
        self.assertEqual(after.egress_bytes_reserved, before.egress_bytes_reserved)

    def test_two_engines_do_not_share_host_functions(self) -> None:
        other = omni.Engine()
        self.addCleanup(other.close)
        self.engine.register_function("mine", lambda: 1.0, arity=0)
        self.assertEqual(self.engine.host_function_count, 1)
        self.assertEqual(other.host_function_count, 0)
        with self.assertRaises(omni.OmniError) as caught:
            other.run("mine()")
        self.assertEqual(caught.exception.kind, "runtime")

    def test_python_recursion_costs_the_engine_no_depth(self) -> None:
        # A callable that recurses in Python stays one call deep: the engine
        # entered it once, and re-entering the engine from inside is refused
        # rather than counted. So the depth guard is about the *program's*
        # nesting, which is the recursion the native stack would otherwise pay for.
        def down(value):
            if value == 0.0:
                return 0.0
            return down(value - 1)

        self.engine.register_function("down", down, arity=1)
        self.assertEqual(self.engine.evaluate("down(500)"), 0.0)

    def test_a_program_that_nests_past_the_guard_is_stopped(self) -> None:
        # MAX_CALL_DEPTH is the reason this test exists at all: without it,
        # `id(id(id(...)))` would grow the native stack until the process died.
        def nest(depth: int) -> str:
            return "id(" * depth + "1" + ")" * depth

        self.engine.register_function("id", lambda x: x, arity=1)
        self.assertEqual(self.engine.evaluate(nest(_ffi.MAX_CALL_DEPTH)), 1.0)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.evaluate(nest(_ffi.MAX_CALL_DEPTH + 1))
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertIn(f"nested {_ffi.MAX_CALL_DEPTH} deep", caught.exception.message)
        # And the counter is per execution: the same program one depth shorter
        # still works after the refusal.
        self.assertEqual(self.engine.evaluate(nest(_ffi.MAX_CALL_DEPTH)), 1.0)

    def test_close_releases_every_registration_it_holds(self) -> None:
        raw = _ffi.RawEngine()
        for index in range(5):
            self.assertEqual(raw.register_host_fn(f"f{index}", 0, lambda: 1.0), _ffi.OMNI_OK)
        self.assertEqual(raw.host_fn_count(), 5)
        raw.close()
        raw.close()  # idempotent, and it must not call into a freed handle twice

    def test_a_registration_with_a_bad_name_is_rejected_locally(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.register_function("n" * 33, lambda: 1.0, arity=0)
        self.assertEqual(caught.exception.kind, "argument")
        self.assertEqual(self.engine.host_function_count, 0)
        # An empty name reaches the library, which refuses it as an argument.
        self.assertEqual(_ffi.RawEngine().register_host_fn("", 0, lambda: 1.0), _ffi.OMNI_ERR_ARG)


class CallSyntax(unittest.TestCase):
    """The grammar Phase 3 added, tested through the friendly wrapper."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)
        self.engine.register_function("id", lambda x: x, arity=1)

    def test_a_call_is_an_expression_not_only_a_statement(self) -> None:
        self.assertEqual(self.engine.evaluate("id(1) + id(2)"), 3.0)
        self.assertTrue(self.engine.run('id("a") == "a"').value)
        self.assertEqual(self.engine.run("-id(2) * id(3)").value, -6.0)

    def test_whitespace_and_a_paren_still_mean_a_call(self) -> None:
        self.assertEqual(self.engine.evaluate("id ( 5 )"), 5.0)

    def test_a_missing_comma_names_the_call(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run('id(1 2)')
        self.assertEqual(caught.exception.kind, "compile")
        self.assertIn("expected ',' or ')'", caught.exception.message)

    def test_an_unclosed_call_is_a_compile_error(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("id(1")
        self.assertEqual(caught.exception.kind, "compile")

    def test_a_semicolon_and_a_comment_after_a_call_are_ignored(self) -> None:
        self.assertEqual(self.engine.evaluate("id(1); // trailing"), 1.0)

    def test_too_many_arguments_in_a_program_is_a_parse_error(self) -> None:
        args = ", ".join("1" for _ in range(_ffi.MAX_ARGS + 1))
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run(f"id({args})")
        self.assertEqual(caught.exception.kind, "compile")
        self.assertIn("at most 16 arguments", caught.exception.message)


class Memory(unittest.TestCase):
    """The counters exist so a host can tell a high-water mark from a leak."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_stats_reports_counters_of_the_right_size(self) -> None:
        stats = self.engine.stats()
        self.assertEqual(stats.execs, 0)
        self.assertEqual(stats.egress_high_water, 0)
        self.engine.run('"one"')
        self.engine.run('"two"')
        stats = self.engine.stats()
        self.assertEqual(stats.execs, 2)
        self.assertGreaterEqual(stats.strings, 2)
        self.assertGreaterEqual(stats.egress_high_water, 3)
        self.assertGreaterEqual(stats.arena_bytes_reserved, 1)

    def test_repeated_string_executions_reserve_no_more_memory(self) -> None:
        program = r'"a string long enough to be interesting"'
        self.engine.run(program)
        before = self.engine.stats()
        for _ in range(5_000):
            self.assertEqual(self.engine.run(program).value, "a string long enough to be interesting")
        after = self.engine.stats()
        self.assertEqual(after.execs, before.execs + 5_000)
        self.assertEqual(after.arena_bytes_reserved, before.arena_bytes_reserved)
        self.assertEqual(after.egress_bytes_reserved, before.egress_bytes_reserved)
        self.assertEqual(after.egress_high_water, before.egress_high_water)

    def test_a_larger_result_grows_the_egress_once_and_then_stops(self) -> None:
        self.engine.define_string("chunk", "x" * (256 * 1024))
        self.engine.run('"small"')
        small = self.engine.stats()
        first = self.engine.run("chunk + chunk")
        self.assertEqual(len(first.text), 512 * 1024)
        grown = self.engine.stats()
        self.assertGreater(grown.egress_bytes_reserved, small.egress_bytes_reserved)
        # Every later call of the same size reuses the same reservation: rewind,
        # not free-then-re-map.
        for _ in range(10):
            self.engine.run("chunk + chunk")
        again = self.engine.stats()
        self.assertEqual(again.egress_bytes_reserved, grown.egress_bytes_reserved)
        self.assertEqual(again.egress_high_water, grown.egress_high_water)

    def test_a_string_over_the_engines_ceiling_is_a_runtime_error(self) -> None:
        # 16 MiB + 1 byte of literal is the program's fault, and the message
        # says how big the ceiling is. Building the same bytes in Python to hand
        # to define_string instead is the host's fault.
        payload = "y" * (_ffi.MAX_STRING_LEN + 1)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run(f'"{payload}"')
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertIn("MiB", caught.exception.message)

        with self.assertRaises(omni.OmniError) as caught:
            self.engine.define_string("s", payload)
        self.assertEqual(caught.exception.kind, "argument")
        self.assertIn("the ABI accepts at most", caught.exception.message)


class Errors(unittest.TestCase):
    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_syntax_error_is_a_compile_error_with_text(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("let = 1")
        self.assertEqual(caught.exception.status, 1)
        self.assertEqual(caught.exception.kind, "compile")
        self.assertIn("[Line 1, Col 5]", caught.exception.message)

    def test_division_by_zero_is_a_runtime_error(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.evaluate("1 / 0")
        self.assertEqual(caught.exception.kind, "runtime")
        # The line number crosses the ABI boundary, so a host can point at source.
        self.assertEqual(caught.exception.message, "[Line 1, Col 3]: division by zero")

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
        self.assertTrue(caught.exception.message.startswith("[Line 2, Col "), caught.exception.message)

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

    def test_try_run_copies_a_string_the_same_way_run_does(self) -> None:
        status, result, message = self.engine.try_run('"no exceptions here"')
        self.assertEqual(status, 0)
        self.assertEqual(message, "")
        self.assertEqual(result.value, "no exceptions here")

    def test_a_success_clears_the_stale_error(self) -> None:
        with self.assertRaises(omni.OmniError):
            self.engine.run("###")
        self.assertTrue(self.engine.last_error)
        self.engine.run("1")
        self.assertEqual(self.engine.last_error, "")


class Batch(unittest.TestCase):
    """One crossing, N programs: the shape of :meth:`Engine.run_many`.

    What a loop of ``run`` cannot give you is N string results alive at the same
    time, and what it costs you is N crossings. Both halves get tested here, plus
    the boundary rules: a batch is independent executions, not one long program,
    and a failing item is data rather than a stopped loop.
    """

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_run_many_returns_one_result_per_source_in_order(self) -> None:
        results = self.engine.run_many(["6 * 7", '"hello, " + "batch"', "1 == 1", ""])
        self.assertEqual([r.tag for r in results], ["number", "string", "bool", "undef"])
        self.assertEqual(results[0].value, 42.0)
        self.assertEqual(results[1].value, "hello, batch")
        self.assertEqual(results[2].value, True)
        self.assertIsNone(results[3].value)

    def test_every_string_of_a_batch_is_copied_out(self) -> None:
        # Three live results, read in the order the ABI wrote them and again in
        # reverse: if the SDK let the engine rewind between items, the earlier
        # texts would be whatever the last evaluation happened to leave there.
        programs = ['"a" + "1"', '"b" + "2"', '"c" + "3"']
        results = self.engine.run_many(programs)
        self.assertEqual([r.text for r in results], [b"a1", b"b2", b"c3"])
        for source, expected in reversed(list(zip(programs, results))):
            self.assertEqual(self.engine.run(source).text, expected.text)

    def test_a_failing_item_raises_naming_its_index(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run_many(["1 + 1", "1 / 0", "2 + 2"])
        self.assertIn("item 1 of 3", str(caught.exception))
        self.assertIn("division by zero", str(caught.exception))
        self.assertEqual(self.engine.last_status, _ffi.OMNI_ERR_RUNTIME)

    def test_try_run_many_keeps_running_after_a_failure(self) -> None:
        items = self.engine.try_run_many(["1 +", "2 + 3", "1 / 0", "20 / 4"])
        self.assertEqual([status for status, _r, _m in items], [1, 0, 2, 0])
        self.assertIsNone(items[0][1])
        self.assertEqual(items[1][1].value, 5.0)
        self.assertEqual(items[3][1].value, 5.0, "the item after a failure ran")
        self.assertIn("unexpected end of input", items[0][2])
        self.assertIn("division by zero", items[2][2])

    def test_an_item_cannot_read_the_previous_items_binding(self) -> None:
        items = self.engine.try_run_many(["let secret = 1; secret", "secret"])
        self.assertEqual(items[0][1].value, 1.0)
        self.assertEqual(items[1][0], _ffi.OMNI_ERR_RUNTIME)
        self.assertIn("secret", items[1][2], "the message is item 1's, not the last one's")

    def test_a_host_function_is_callable_from_a_batch_item(self) -> None:
        self.engine.register_function("square", lambda x: x * x, arity=1)
        self.engine.register_function("tag", lambda x: f"squared: {x * x:g}", arity=1)
        results = self.engine.run_many(["square(6)", "square(7) + 1", 'tag(3) + "!"'])
        self.assertEqual([results[0].value, results[1].value], [36.0, 50.0])
        self.assertEqual(results[2].value, "squared: 9!")

    def test_an_empty_batch_is_an_empty_list_that_costs_no_execution(self) -> None:
        held = self.engine.run('"still current"')
        before = self.engine.stats()
        self.assertEqual(self.engine.try_run_many([]), [])
        self.assertEqual(self.engine.run_many([]), [])
        after = self.engine.stats()
        self.assertEqual(after.execs, before.execs, "a no-op crossed nothing")
        self.assertEqual(held.text, b"still current")

    def test_execs_counts_items_not_crossings(self) -> None:
        before = self.engine.stats().execs
        self.engine.run_many(["1", "2", "3", "4", "5"])
        self.assertEqual(self.engine.stats().execs, before + 5)

    def test_a_batch_larger_than_the_abi_allows_is_refused_locally(self) -> None:
        sources = ["1"] * (_ffi.MAX_BATCH + 1)
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run_many(sources)
        self.assertIn(str(_ffi.MAX_BATCH), str(caught.exception))
        self.assertEqual(self.engine.stats().execs, 0, "nothing ran")

    def test_an_over_long_source_refuses_the_whole_batch(self) -> None:
        # Checked per item, before the crossing, so item 0 never runs either.
        long = "1" + " " * (_ffi.MAX_SOURCE_LEN + 1)
        with self.assertRaises(omni.OmniError):
            self.engine.run_many(["1 + 1", long])
        self.assertEqual(self.engine.stats().execs, 0)

    def test_batch_error_answers_only_the_current_batch(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        raw.exec_many(["1 + 1", "let = 2"])
        self.assertEqual(raw.batch_error(0), "")
        self.assertIn("expected a name", raw.batch_error(1))
        self.assertEqual(raw.batch_error(2), "", "an index past the batch")
        # A single evaluation ends the batch's currency: there is no item 1 of a
        # call that had no items.
        raw.exec("1")
        self.assertEqual(raw.batch_error(1), "")

    def test_repeated_batches_reserve_a_flat_amount_of_memory(self) -> None:
        programs = ['"a" + "b" + "c"'] * 8
        self.engine.run_many(programs)
        before = self.engine.stats()
        for _ in range(2_000):
            self.assertEqual(len(self.engine.run_many(programs)), 8)
        after = self.engine.stats()
        self.assertEqual(after.execs, before.execs + 16_000)
        self.assertEqual(after.egress_bytes_reserved, before.egress_bytes_reserved)
        self.assertEqual(after.arena_bytes_reserved, before.arena_bytes_reserved)
        self.assertEqual(after.egress_high_water, before.egress_high_water)

    def test_a_batch_of_long_strings_stops_growing(self) -> None:
        # Eight 40 KiB results need more than one arena chunk, which is the case
        # a single call never reaches and the reason the batch shares one rewind.
        programs = ['"' + "x" * (40 * 1024) + '"'] * 8
        self.engine.run_many(programs)
        before = self.engine.stats()
        self.assertGreaterEqual(before.egress_bytes_reserved, 8 * 40 * 1024)
        for _ in range(50):
            self.engine.run_many(programs)
        after = self.engine.stats()
        self.assertEqual(after.egress_bytes_reserved, before.egress_bytes_reserved, "reused")

    def test_the_raw_layer_hands_back_arrays_the_caller_sizes(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        status, values, statuses = raw.exec_many(["1 + 1", "no_such_function()"])
        self.assertEqual(status, _ffi.OMNI_ERR_RUNTIME)
        self.assertEqual(len(values), 2)
        self.assertEqual(statuses, [0, _ffi.OMNI_ERR_RUNTIME])
        self.assertEqual(raw.read_string(values[0]), b"")
        self.assertEqual(values[0].number, 2.0)


class Parallel(unittest.TestCase):
    """One crossing, N engines: the shape of :meth:`Engine.run_parallel`.

    The parallel entry point must not change an answer, only where the item ran.
    These tests therefore compare it against the sequential batch as much as they
    exercise it, and cover the one thing the SDK adds on top of the ABI: refusing
    a bad ``max_threads`` locally, and falling back when the loaded library
    predates the export.
    """

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_run_parallel_returns_one_result_per_source_in_order(self) -> None:
        results = self.engine.run_parallel(
            ["6 * 7", '"hello, " + "parallel"', "1 == 1", ""], max_threads=4
        )
        self.assertEqual([r.tag for r in results], ["number", "string", "bool", "undef"])
        self.assertEqual(results[0].value, 42.0)
        self.assertEqual(results[1].value, "hello, parallel")
        self.assertEqual(results[2].value, True)
        self.assertIsNone(results[3].value)

    def test_a_failing_item_raises_naming_its_index(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run_parallel(["1 + 1", "1 / 0", "2 + 2"], max_threads=4)
        self.assertIn("item 1 of 3", str(caught.exception))
        self.assertIn("division by zero", str(caught.exception))
        self.assertEqual(
            self.engine.stats().execs, 3, "the item after the failure ran"
        )

    def test_try_run_parallel_keeps_running_after_a_failure(self) -> None:
        items = self.engine.try_run_parallel(
            ["1 +", "2 + 3", "1 / 0", "20 / 4"], max_threads=4
        )
        self.assertEqual([status for status, _r, _m in items], [1, 0, 2, 0])
        self.assertIsNone(items[0][1])
        self.assertEqual(items[1][1].value, 5.0)
        self.assertEqual(items[3][1].value, 5.0, "the item after a failure ran")
        self.assertIn("unexpected end of input", items[0][2])
        self.assertIn("division by zero", items[2][2])

    def test_zero_and_one_threads_never_change_an_answer(self) -> None:
        sources = ["1 + 1", "1 / 0", "2 * 2", 'len("abcd")', "40 + 2"]
        auto = self.engine.try_run_parallel(sources, max_threads=0)
        one = self.engine.try_run_parallel(sources, max_threads=1)
        self.assertEqual(
            [(status, None if result is None else result.value) for status, result, _ in auto],
            [(status, None if result is None else result.value) for status, result, _ in one],
        )
        self.assertEqual(auto[4][1].value, 42.0)

    def test_every_string_of_a_parallel_batch_is_copied_out(self) -> None:
        programs = ['"a" + "1"', '"b" + "2"', '"c" + "3"', '"d" + "4"']
        results = self.engine.run_parallel(programs, max_threads=4)
        self.assertEqual([r.text for r in results], [b"a1", b"b2", b"c3", b"d4"])
        # Read again in reverse after an independent execution: every Result was
        # copied out, so none of them points at the shared egress.
        self.engine.run("0")
        self.assertEqual([r.text for r in reversed(results)], [b"d4", b"c3", b"b2", b"a1"])

    def test_a_host_function_is_callable_from_a_worker(self) -> None:
        self.engine.register_function("square", lambda x: x * x, arity=1)
        results = self.engine.run_parallel(
            ["square(6)", "square(7) + 1", "square(21)"], max_threads=4
        )
        self.assertEqual([r.value for r in results], [36.0, 50.0, 441.0])

    def test_a_non_uint32_max_threads_is_refused_locally(self) -> None:
        for bad in (True, -1, 1.5, "4", 1 << 32):
            with self.assertRaises(omni.OmniError) as caught:
                self.engine.run_parallel(["1 + 1"], max_threads=bad)  # type: ignore[arg-type]
            self.assertIn("max_threads", str(caught.exception))
        self.assertEqual(self.engine.stats().execs, 0, "nothing crossed")

    def test_a_library_without_the_parallel_symbol_falls_back(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        saved = raw._lib.omni_exec_parallel
        raw._lib.omni_exec_parallel = None
        try:
            status, values, statuses = raw.exec_parallel(["1 + 1", "2 + 2"], 4)
        finally:
            raw._lib.omni_exec_parallel = saved
        self.assertEqual(status, _ffi.OMNI_OK)
        self.assertEqual(statuses, [0, 0])
        self.assertEqual([value.number for value in values], [2.0, 4.0])

    def test_an_empty_parallel_batch_is_an_empty_list(self) -> None:
        held = self.engine.run('"still current"')
        before = self.engine.stats()
        self.assertEqual(self.engine.try_run_parallel([], max_threads=4), [])
        self.assertEqual(self.engine.run_parallel([], max_threads=4), [])
        self.assertEqual(self.engine.stats().execs, before.execs)
        self.assertEqual(held.text, b"still current")


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

    def test_a_panicking_engine_survives_a_string_too(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        _, before = raw.exec('"alive"')
        self.assertEqual(raw.read_string(before), b"alive")
        self.assertEqual(raw.debug_panic(), 3)
        status, after = raw.exec('"still alive"')
        self.assertEqual(status, 0)
        self.assertEqual(raw.read_string(after), b"still alive")

    def test_error_text_is_copied_out_and_truncates(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        raw.exec("1 / 0")  # "[Line 1, Col 3]: division by zero"
        self.assertEqual(raw.last_error(cap=100), "[Line 1, Col 3]: division by zero")
        # The ABI writes at most cap - 1 bytes so there is always room for a NUL.
        self.assertEqual(raw.last_error(cap=8), "[Line 1")
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

    def test_a_wheel_bundled_engine_is_searched_before_the_workspace(self) -> None:
        # A release wheel stages the artifact in omni/_native. It has to win
        # over any target/ directory above the installed package, or a checkout
        # that happens to sit nearby would serve a different build.
        paths = _ffi._candidate_paths()
        name = _ffi.library_file_name()
        packaged = _ffi.os.path.dirname(_ffi.os.path.abspath(_ffi.__file__))
        bundled = _ffi.os.path.join(packaged, "_native", name)
        self.assertIn(bundled, paths)
        target_at = next(
            i for i, p in enumerate(paths) if f"{_ffi.os.sep}target{_ffi.os.sep}" in p
        )
        self.assertLess(paths.index(bundled), target_at)

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
        self.assertEqual(lib.omni_abi_version(), _ffi.OMNI_ABI_VERSION)

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

    def test_a_library_with_a_different_abi_is_refused(self) -> None:
        class Fake:
            def omni_abi_version(self) -> int:
                return 1

        with self.assertRaises(omni.OmniLibraryError) as caught:
            _ffi._checked(Fake(), "stale-omni.dll")
        self.assertIn("ABI version 1", str(caught.exception))
        self.assertIn(f"ABI version {_ffi.OMNI_ABI_VERSION}", str(caught.exception))
        self.assertIn("cargo build -p omni-ffi", str(caught.exception))

    def test_a_library_without_an_abi_version_export_is_refused(self) -> None:
        class Phase1:  # a v1 artifact has no such symbol at all
            def __getattr__(self, name: str):
                raise AttributeError(name)

        with self.assertRaises(omni.OmniLibraryError) as caught:
            _ffi._checked(Phase1(), "omni.dll")
        self.assertIn("predates the ABI", str(caught.exception))

    def test_the_current_abi_is_accepted(self) -> None:
        class Fake:
            def omni_abi_version(self) -> int:
                return _ffi.OMNI_ABI_VERSION

        self.assertIsInstance(_ffi._checked(Fake(), "omni.dll"), Fake)


class AbiLayout(unittest.TestCase):
    def test_value_struct_matches_the_c_header(self) -> None:
        self.assertEqual(_ffi.ctypes.sizeof(_ffi.OmniPayload), 8)
        self.assertEqual(_ffi.ctypes.sizeof(_ffi.OmniValue), 16)
        self.assertEqual(_ffi.OmniValue.tag.offset, 0)
        self.assertEqual(_ffi.OmniValue.len.offset, 4)
        self.assertEqual(_ffi.OmniValue.payload.offset, 8)

    def test_stats_struct_matches_the_c_header(self) -> None:
        self.assertEqual(_ffi.ctypes.sizeof(_ffi.OmniStats), 72)
        self.assertEqual(_ffi.OmniStats.size.offset, 0)
        self.assertEqual(_ffi.OmniStats.execs.offset, 8)
        self.assertEqual(_ffi.OmniStats.egress_high_water.offset, 64)

    def test_profile_struct_matches_the_c_header(self) -> None:
        self.assertEqual(_ffi.OMNI_MAX_OPCODES, 64)
        self.assertEqual(_ffi.ctypes.sizeof(_ffi.OmniProfile), 1064)
        self.assertEqual(_ffi.OmniProfile.size.offset, 0)
        self.assertEqual(_ffi.OmniProfile.opcode_count.offset, 4)
        self.assertEqual(_ffi.OmniProfile.total_instructions.offset, 8)
        self.assertEqual(_ffi.OmniProfile.elapsed_ns.offset, 16)
        self.assertEqual(_ffi.OmniProfile.peak_bytes.offset, 24)
        self.assertEqual(_ffi.OmniProfile.reserved.offset, 32)
        self.assertEqual(_ffi.OmniProfile.opcode_calls.offset, 40)
        self.assertEqual(_ffi.OmniProfile.opcode_ns.offset, 552)

    def test_number_and_ptr_arms_share_the_same_eight_bytes(self) -> None:
        raw = _ffi.OmniValue()
        raw.number = 1.0
        # The same eight bytes, read two ways -- which is why reading the wrong
        # arm is a silent nonsense value rather than an error.
        self.assertEqual(raw.ptr, int.from_bytes(struct.pack("<d", 1.0), "little"))
        raw.ptr = None
        self.assertEqual(raw.number, 0.0)

    def test_status_and_tag_names(self) -> None:
        self.assertEqual(omni.status_name(0), "ok")
        self.assertEqual(omni.status_name(4), "argument")
        self.assertEqual(omni.status_name(99), "status-99")
        self.assertEqual(omni.tag_name(0), "number")
        self.assertEqual(omni.tag_name(3), "undef")
        self.assertEqual(omni.tag_name(4), "string")
        self.assertEqual(omni.tag_name(_ffi.OMNI_STRING), "string")
        self.assertEqual(omni.tag_name(9), "tag-9")

    def test_bool_and_undef_values_convert(self) -> None:
        # The grammar has no literals for these, so build the struct directly.
        raw = _ffi.OmniValue()
        raw.tag, raw.number = _ffi.OMNI_BOOL, 1.0
        self.assertEqual(omni._convert(raw).value, True)
        raw.tag, raw.number = _ffi.OMNI_UNDEF, 0.0
        self.assertIsNone(omni._convert(raw).value)

    def test_a_string_converts_with_its_copied_bytes(self) -> None:
        raw = _ffi.OmniValue()
        raw.tag, raw.len, raw.ptr = _ffi.OMNI_STRING, 5, 0x1000
        result = omni._convert(raw, b"hello")
        self.assertEqual(result.tag, "string")
        self.assertEqual(result.value, "hello")
        self.assertEqual(result.number, 0.0)  # never an address posing as a float

    def test_a_string_that_is_not_valid_utf8_still_reports_its_bytes(self) -> None:
        # `text` is always the exact bytes, so a caller can report an engine bug
        # with evidence instead of only seeing replacement characters.
        raw = _ffi.OmniValue()
        raw.tag, raw.len = _ffi.OMNI_STRING, 2
        self.assertEqual(omni._convert(raw, b"\xff\xfe").text, b"\xff\xfe")
        self.assertTrue(omni._convert(raw, b"\xff\xfe").value)


class Builtins(unittest.TestCase):
    """The standard helpers every host gets without registering anything."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_math_builtins(self) -> None:
        e = self.engine
        self.assertEqual(e.evaluate("abs(-3)"), 3.0)
        self.assertEqual(e.evaluate("min(2, 5)"), 2.0)
        self.assertEqual(e.evaluate("max(2, 5)"), 5.0)
        self.assertEqual(e.evaluate("clamp(12, 0, 10)"), 10.0)
        self.assertEqual(e.evaluate("floor(2.9)"), 2.0)
        self.assertEqual(e.evaluate("ceil(2.1)"), 3.0)
        self.assertEqual(e.evaluate("round(2.5)"), 3.0)
        self.assertEqual(e.evaluate("-round(-2.5)"), 3.0)
        self.assertEqual(e.evaluate("pow(2, 10)"), 1024.0)
        self.assertEqual(e.evaluate("sqrt(16)"), 4.0)

    def test_string_builtins(self) -> None:
        e = self.engine
        self.assertEqual(e.string(r'trim("  padded\t")'), "padded")
        self.assertEqual(e.string('upper("MiXeD")'), "MIXED")
        self.assertEqual(e.string('lower("MiXeD")'), "mixed")
        self.assertEqual(e.string(r'upper("stra\u{00DF}e")'), "STRASSE")
        self.assertEqual(e.evaluate(r'len("h\u{00E9}llo")'), 6.0, "é is two bytes")
        self.assertEqual(e.string(r'upper(trim("  hi  "))'), "HI")
        self.assertIs(e.eval('contains("omni engine", "engine")'), True)
        self.assertIs(e.eval('contains("omni", "zz")'), False)
        self.assertEqual(e.string('replace("a-b-a", "-", "+")'), "a+b+a")
        self.assertEqual(e.string('replace("abc", "", "x")'), "abc")
        # Character offsets, so a multi-byte character is never cut in half.
        self.assertEqual(e.string(r'slice("h\u{00E9}llo", 1, 3)'), "\u00e9l")
        self.assertEqual(e.string('slice("hello", 2, 99)'), "llo")
        self.assertEqual(e.string('slice("hello", 3, 1)'), "")

    def test_type_of_names_number_string_boolean_and_null(self) -> None:
        e = self.engine
        self.assertEqual(e.eval("type_of(2)"), "number")
        self.assertEqual(e.eval('type_of("x")'), "string")
        self.assertEqual(e.eval("type_of(1 == 1)"), "boolean")
        e.register_function("nothing", lambda: None, arity=0)
        self.assertEqual(e.eval("type_of(nothing())"), "null")

    def test_a_wrong_type_names_the_builtin_and_the_location(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run('abs("x")')
        self.assertEqual(caught.exception.kind, "runtime")
        self.assertTrue(
            caught.exception.message.startswith(
                "[Line 1, Col 1]: type mismatch: abs() needs a number"
            ),
            caught.exception.message,
        )

    def test_a_wrong_arity_names_the_call(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("min(1)")
        self.assertEqual(
            caught.exception.message,
            "[Line 1, Col 1]: Mismatched arity in call 'min': takes 2 arguments, got 1",
        )

    def test_clamp_refuses_an_inverted_range(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run("clamp(1, 10, 0)")
        self.assertIn("lower bound", caught.exception.message)

    def test_a_registered_function_shadows_a_builtin(self) -> None:
        self.engine.register_function("abs", lambda _: 99.0, arity=1)
        self.assertEqual(self.engine.evaluate("abs(-1)"), 99.0)
        self.assertTrue(self.engine.unregister_function("abs"))
        self.assertEqual(self.engine.evaluate("abs(-1)"), 1.0)


class EvalStr(unittest.TestCase):
    """``eval_str`` returns Python data, not a Result."""

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_whole_numbers_become_int(self) -> None:
        result = self.engine.eval_str("1 + 1")
        self.assertIsInstance(result, int)
        self.assertEqual(result, 2)

    def test_fractional_numbers_stay_float(self) -> None:
        result = self.engine.eval_str("1 / 2")
        self.assertIsInstance(result, float)
        self.assertEqual(result, 0.5)

    def test_strings_booleans_and_nothing_come_back_native(self) -> None:
        self.assertEqual(self.engine.eval_str('"a" + "b"'), "ab")
        self.assertIs(self.engine.eval_str("1 == 1"), True)
        self.assertIsNone(self.engine.eval_str(""))

    def test_eval_is_the_same_unwrapping_under_the_node_name(self) -> None:
        result = self.engine.eval("40 + 2")
        self.assertIsInstance(result, int)
        self.assertEqual(result, 42)
        self.assertEqual(self.engine.eval('"hi"'), "hi")
        self.assertIs(self.engine.eval("1 == 1"), True)
        self.assertIsNone(self.engine.eval(""))

    def test_an_error_still_raises(self) -> None:
        with self.assertRaises(omni.OmniError):
            self.engine.eval_str("1 / 0")


class PositionsAndTimer(unittest.TestCase):
    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_an_error_on_the_second_line_reports_both_numbers(self) -> None:
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run('let a = 1\nlet b = "x" + 2')
        self.assertEqual(
            caught.exception.message,
            '[Line 2, Col 13]: type mismatch: cannot add string to number: "x" + 2',
        )

    def test_columns_count_bytes(self) -> None:
        # The '+' after a two-byte 'é' is at byte column 6, not character column 5.
        with self.assertRaises(omni.OmniError) as caught:
            self.engine.run('"\u00e9" + 1')
        self.assertIn("[Line 1, Col 6]", caught.exception.message)

    def test_stats_carry_the_last_execution_timer(self) -> None:
        self.assertEqual(self.engine.stats().last_exec_micros, 0)
        self.engine.run('"' + "x" * 200_000 + '"')
        stats = self.engine.stats()
        self.assertGreater(stats.last_exec_micros, 0)
        self.assertEqual(stats.execs, 1)
        self.assertIsInstance(stats.last_exec_micros, int)


class Profiling(unittest.TestCase):
    """Phase 9's diagnostics: per-opcode heatmaps over the Python boundary."""

    MNEMONICS = [
        "PUSH_NUMBER",
        "PUSH_STR",
        "PUSH_FOLDED",
        "PUSH_NIL",
        "PUSH_UNDEF",
        "LOAD",
        "STORE",
        "POP",
        "ADD",
        "SUB",
        "MUL",
        "DIV",
        "MOD",
        "EQ",
        "NE",
        "NEG",
        "JUMP",
        "JUMP_IF_FALSE",
        "CALL",
        "RET",
        "IMPORT",
    ]

    def setUp(self) -> None:
        self.engine = omni.Engine()
        self.addCleanup(self.engine.close)

    def test_a_fresh_engine_profiles_as_all_zeros(self) -> None:
        profile = self.engine.profile()
        self.assertEqual(profile.total_instructions, 0)
        self.assertEqual(profile.elapsed_ns, 0)
        self.assertEqual(profile.peak_bytes, 0)
        # Every opcode this build knows is present, even the unused ones, so a
        # heatmap sums the same way on every build.
        self.assertEqual(set(profile.opcode_calls), set(self.MNEMONICS))
        self.assertEqual(set(profile.opcode_ns), set(self.MNEMONICS))
        self.assertEqual(sum(profile.opcode_calls.values()), 0)
        self.assertEqual(sum(profile.opcode_ns.values()), 0)

    def test_the_heatmap_sums_to_the_instruction_count(self) -> None:
        result = self.engine.run("let x = 2 + 3\nx * 4")
        self.assertEqual(result.value, 20.0)
        profile = self.engine.profile()
        self.assertEqual(sum(profile.opcode_calls.values()), profile.total_instructions)
        self.assertGreater(profile.total_instructions, 0)
        self.assertEqual(profile.opcode_calls["ADD"], 0)  # folded at compile time
        self.assertEqual(profile.opcode_calls["PUSH_FOLDED"], 1)
        self.assertEqual(profile.opcode_calls["STORE"], 1)
        self.assertEqual(profile.opcode_calls["LOAD"], 1)
        self.assertEqual(profile.opcode_calls["MUL"], 1)

    def test_per_opcode_timing_stays_zero_until_enabled(self) -> None:
        self.engine.run("1 + 1")
        profile = self.engine.profile()
        self.assertEqual(sum(profile.opcode_ns.values()), 0)
        self.assertGreater(profile.elapsed_ns, 0)  # the scalar timer is always on
        self.engine.set_profiling(True)
        self.engine.run("let x = 1\nx" + " + 1" * 2_000)
        profile = self.engine.profile()
        self.assertGreater(sum(profile.opcode_ns.values()), 0)
        self.assertLessEqual(sum(profile.opcode_ns.values()), profile.elapsed_ns)
        self.engine.set_profiling(False)
        self.engine.run("let x = 1\nx" + " + 1" * 2_000)
        self.assertEqual(sum(self.engine.profile().opcode_ns.values()), 0)

    def test_peak_bytes_bounds_a_string_heavy_program(self) -> None:
        self.engine.run("1 + 2")
        self.assertEqual(self.engine.profile().peak_bytes, 0)
        text = '"' + "x" * 64_000 + '"'
        result = self.engine.run(text)
        self.assertEqual(len(result.value), 64_000)
        profile = self.engine.profile()
        self.assertGreaterEqual(profile.peak_bytes, 64_000)
        self.assertLess(profile.peak_bytes, 2 * 64_000)

    def test_the_profile_is_per_execution(self) -> None:
        self.engine.run('"' + "x" * 64_000 + '"')
        self.engine.run("7")
        profile = self.engine.profile()
        self.assertEqual(profile.total_instructions, 2)  # PUSH_NUMBER + RET
        self.assertEqual(profile.peak_bytes, 0)

    def test_a_failed_execution_is_profiled_like_a_successful_one(self) -> None:
        self.engine.set_profiling(True)
        with self.assertRaises(omni.OmniError):
            self.engine.run("1 / 0")
        profile = self.engine.profile()
        self.assertEqual(profile.opcode_calls["DIV"], 1)
        self.assertEqual(sum(profile.opcode_calls.values()), profile.total_instructions)
        self.assertGreater(sum(profile.opcode_ns.values()), 0)

    def test_a_batch_profiles_its_last_item(self) -> None:
        results = self.engine.run_many(["2 + 21", "2 * 21"])
        self.assertEqual([item.value for item in results], [23.0, 42.0])
        profile = self.engine.profile()
        self.assertEqual(profile.total_instructions, 2)  # PUSH_FOLDED + RET
        self.assertEqual(profile.opcode_calls["PUSH_FOLDED"], 1)

    def test_the_raw_binding_reports_the_same_numbers(self) -> None:
        raw = _ffi.RawEngine()
        self.addCleanup(raw.close)
        raw.set_profiling(True)
        status, value = raw.exec("2 + 3")
        self.assertEqual(status, _ffi.OMNI_OK)
        profile = raw.profile()
        self.assertEqual(profile.total_instructions, 2)
        self.assertEqual(profile.opcode_calls["PUSH_FOLDED"], 1)
        self.assertEqual(profile.opcode_calls["ADD"], 0)
        self.assertGreater(profile.elapsed_ns, 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
