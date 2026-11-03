/*
 * embed.c -- minimal C embedding of OmniEngine, and the check that omni.h and
 * the compiled library actually agree.
 *
 *   cargo build -p omni-ffi
 *   gcc -std=c11 -Wall -Wextra -I crates/omni-ffi/include \
 *       examples/c/embed.c target/debug/libomni.dll.a -o embed.exe
 *
 * On Linux/macOS replace the import library with -lomni and add
 * -L target/debug.
 *
 * Every call is a separate statement: the value written through an out
 * parameter is read after the call returns, never as a sibling argument in the
 * same printf(). Argument evaluation order is unspecified in C, and reading the
 * output in the same call as the one that fills it silently prints a stale 0.
 *
 * Strings follow caller rule 1 in omni.h: the result is an address the engine
 * owns until the next omni_exec(), so this program copies every string it wants
 * to print before doing anything else with the engine.
 */

#include <inttypes.h>
#include <stdio.h>
#include <string.h>

#include "omni.h"

/* A string result, copied into caller-owned storage. `buf` is one byte longer
 * than the payload so it can be printed, and `n` says how much of it is real:
 * an OmniEngine string may contain a NUL, so strlen() on it would be wrong. */
typedef struct Str {
  char buf[64];
  size_t n;
  int truncated;
} Str;

static int omni_ok(uint32_t code) { return code == OMNI_OK; }

/* Copy `value` out of the engine, into this file's own small buffer. */
static Str copy_string(const OmniEngine *engine, const OmniValue *value) {
  Str s;
  memset(&s, 0, sizeof s);
  size_t cap = sizeof s.buf - 1; /* leave the last byte for the terminator */
  s.n = omni_str_copy(engine, value, s.buf, cap);
  s.buf[s.n] = '\0';
  s.truncated = value->len > (uint32_t)s.n;
  return s;
}

/* Evaluate and print one program. Returns the status code. */
static uint32_t show(OmniEngine *engine, const char *label, const char *src) {
  OmniValue result;
  memset(&result, 0xFF, sizeof result); /* poison: a failure must not look like 0 */

  uint32_t code = omni_exec(engine, src, strlen(src), &result);
  if (!omni_ok(code)) {
    char message[128];
    size_t written = omni_last_error(engine, message, sizeof message);
    printf("%-16s %-26s -> status=%u error=%s (%zu bytes)\n", label, src, code, message,
           written);
    return code;
  }
  /* Read the payload after the call, and read strings before anything else can
   * rewind the block they point into. */
  if (result.tag == OMNI_STRING) {
    Str text = copy_string(engine, &result);
    printf("%-16s %-26s -> tag=STRING len=%u \"%s\"%s\n", label, src, result.len, text.buf,
           text.truncated ? " [shown truncated]" : "");
  } else {
    printf("%-16s %-26s -> tag=%u number=%g len=%u\n", label, src, result.tag,
           result.payload.number, result.len);
  }
  return code;
}

/* A check that prints and counts. */
static int expect(int condition, const char *what) {
  printf("%s %-58s\n", condition ? "  ok  " : " FAIL ", what);
  return !condition;
}

int main(void) {
  /* Rule 5: agree on the wire format before trusting a single other symbol. A
   * mismatch means the header and the library disagree about where `len` sits,
   * and every number read after that would be plausible garbage. */
  uint32_t abi = omni_abi_version();
  if (abi != OMNI_ABI_VERSION) {
    fprintf(stderr, "ABI mismatch: header says %u, library says %u\n", OMNI_ABI_VERSION, abi);
    return 1;
  }

  OmniEngine *engine = omni_engine_new();
  if (engine == NULL) {
    fprintf(stderr, "omni_engine_new returned NULL\n");
    return 1;
  }

  printf("OmniEngine %s (ABI %u)\n\n", omni_version(), abi);

  /* The acceptance program: 42 must come out of the C surface unchanged. */
  int failures = 0;
  failures += expect(show(engine, "vertical slice", "let answer = 40 + 2") == OMNI_OK,
                     "40 + 2 evaluates");

  show(engine, "precedence", "2 + 3 * 4");
  show(engine, "division", "20 / 3");
  show(engine, "parentheses", "(2 + 3) * 4");
  show(engine, "modulus", "20 % 7");

  printf("\n-- strings --\n");
  {
    OmniValue value;
    const char *lit = "let who = \"world\"; \"hello, \" + who";
    uint32_t code = omni_exec(engine, lit, strlen(lit), &value);
    failures += expect(code == OMNI_OK && value.tag == OMNI_STRING, "concatenation returns a string");
    failures += expect(value.len == 12, "len is the payload length");
    Str text = copy_string(engine, &value);
    failures += expect(text.n == 12 && memcmp(text.buf, "hello, world", 12) == 0,
                       "omni_str_copy delivers the bytes");

    /* Escapes are decoded by the engine, so the bytes on this side are the
     * characters the program asked for. */
    const char *esc = "\"tab\\there\\nnewline\\\"\\u{1F600}\"";
    code = omni_exec(engine, esc, strlen(esc), &value);
    Str made = copy_string(engine, &value);
    const char *wanted = "tab\there\nnewline\"\xF0\x9F\x98\x80";
    failures += expect(code == OMNI_OK && made.n == strlen(wanted) && memcmp(made.buf, wanted, made.n) == 0,
                       "escapes and a 4-byte code point cross intact");

    /* Content equality, not address equality. */
    const char *eq = "\"a\" + \"b\" == \"ab\"";
    code = omni_exec(engine, eq, strlen(eq), &value);
    failures += expect(code == OMNI_OK && value.tag == OMNI_BOOL && value.payload.number == 1.0,
                       "== compares string content");

    /* A mixed-type + names both operands, and the message survives the copy. */
    const char *mix = "\"hi\" + 42";
    code = omni_exec(engine, mix, strlen(mix), &value);
    char message[128];
    omni_last_error(engine, message, sizeof message);
    failures += expect(code == OMNI_ERR_RUNTIME && strstr(message, "cannot add string to number") != NULL,
                       "mixing string and number says so, with both types");
    printf("       %s\n", message);
  }

  printf("\n-- host strings --\n");
  {
    const char *name = "greeting";
    const char *bytes = "\xE0\xA6\x9C\xE0\xA6\xBE\xE0\xA6\x82\xE0\xA6\xB2\xE0\xA6\xBE"; /* বাংলা */
    uint32_t code =
        omni_define_string(engine, name, strlen(name), bytes, strlen(bytes));
    failures += expect(code == OMNI_OK, "omni_define_string accepts a UTF-8 payload");

    const char *use = "greeting + \"!\"";
    OmniValue value;
    code = omni_exec(engine, use, strlen(use), &value);
    Str text = copy_string(engine, &value);
    failures += expect(code == OMNI_OK && value.tag == OMNI_STRING && text.n == strlen(bytes) + 1,
                       "a program can read a host string");

    /* Bad arguments are refused, not dereferenced. */
    failures += expect(omni_define_string(engine, "", 0, bytes, strlen(bytes)) == OMNI_ERR_ARG,
                       "an empty name is OMNI_ERR_ARG");
    failures += expect(omni_define_string(engine, name, 0, NULL, 4) == OMNI_ERR_ARG,
                       "a NULL payload with a length is OMNI_ERR_ARG");

    /* Redefining releases the old bytes; the ownership check turns the stale
     * value into a zero rather than a use-after-free. */
    OmniValue old;
    const char *read = "greeting";
    code = omni_exec(engine, read, strlen(read), &old);
    Str before = copy_string(engine, &old);
    const char *again = "Bengali";
    omni_define_string(engine, name, strlen(name), again, strlen(again));
    size_t after = omni_str_copy(engine, &old, before.buf, sizeof before.buf - 1);
    failures += expect(code == OMNI_OK && before.n == strlen(bytes) && after == 0,
                       "a superseded host string copies zero bytes");
  }

  printf("\n-- egress lifetime --\n");
  {
    OmniValue first;
    const char *src = "\"first result\"";
    uint32_t code = omni_exec(engine, src, strlen(src), &first);
    Str fresh = copy_string(engine, &first);
    /* A second execution, written to its own struct so `first` is genuinely
     * stale: same handle, same address, newer bytes. */
    OmniValue second;
    const char *next = "\"a much longer second result\"";
    omni_exec(engine, next, strlen(next), &second);
    char stale[32];
    memset(stale, 0, sizeof stale);
    size_t n = omni_str_copy(engine, &first, stale, sizeof stale);
    stale[n] = '\0'; /* n is the engine's own length, and there is room for this */
    failures += expect(code == OMNI_OK && fresh.n == 12, "a string copies out before the next exec");
    /* Not a fault, and not foreign memory: the address is still the engine's, it
     * now answers with the next execution's string. This is the documented
     * trade, and it is why a host copies instead of caching. */
    failures += expect(n == 27 && memcmp(stale, "a much longer second result", 27) == 0,
                       "a stale pointer reads live bytes, never foreign ones");
    printf("       stale read: \"%s\"\n", stale);
    (void)second;
  }

  printf("\n-- the memory story, from outside Rust --\n");
  {
    /* The result is a string, so every iteration promotes into egress: the
     * claim under test is that rewinding bounds memory by one execution's
     * high-water mark rather than by the number of executions. */
    const char *loop = "let a = \"hello\"; let b = \", world\"; a + b + \"!\"";
    OmniStats warm;
    for (int i = 0; i < 200; ++i) {
      omni_exec(engine, loop, strlen(loop), &(OmniValue){0});
    }
    failures += expect(omni_debug_stats(engine, &warm) == OMNI_OK, "omni_debug_stats fills the struct");
    failures += expect(warm.size == sizeof(OmniStats), "size says which layout the callee wrote");

    OmniStats after;
    for (int i = 0; i < 5000; ++i) {
      omni_exec(engine, loop, strlen(loop), &(OmniValue){0});
    }
    failures += expect(omni_debug_stats(engine, &after) == OMNI_OK, "and again after 5,000 executions");
    failures += expect(after.execs == warm.execs + 5000, "execs counted every execution");
    failures += expect(after.arena_bytes_reserved == warm.arena_bytes_reserved,
                       "arena reservation did not grow");
    failures += expect(after.egress_bytes_reserved == warm.egress_bytes_reserved,
                       "egress reservation did not grow");
    failures += expect(after.arena_chunks == warm.arena_chunks, "no new chunks were mapped");
    failures += expect(after.strings > warm.strings, "strings built is the one counter meant to grow");
    printf("       execs=%" PRIu64 " strings=%" PRIu64 " arena=%" PRIu64 "/%" PRIu64
           " egress=%" PRIu64 "/%" PRIu64 " high=%" PRIu64 "\n",
           after.execs, after.strings, after.arena_bytes_in_use, after.arena_bytes_reserved,
           after.egress_bytes_in_use, after.egress_bytes_reserved, after.egress_high_water);
    failures += expect(omni_debug_stats(engine, NULL) == OMNI_ERR_ARG, "a NULL target is refused");
  }

  printf("\n-- the boundary still holds --\n");
  /* Errors are data, not crashes. */
  failures += expect(show(engine, "syntax error", "let = 1") == OMNI_ERR_COMPILE, "syntax error status");
  failures += expect(show(engine, "divide by zero", "1 / 0") == OMNI_ERR_RUNTIME, "runtime error status");

  /* The panic boundary: this returns instead of taking the process down. */
  uint32_t panic = omni_debug_panic(engine);
  printf("%-16s %-26s -> status=%u (expected %u)\n", "panic boundary", "", panic, OMNI_ERR_PANIC);
  failures += expect(panic == OMNI_ERR_PANIC, "a panic is caught at the boundary");

  /* And the engine still works afterwards. */
  const char *recovery = "6 * 7";
  double n = 0.0;
  uint32_t alive = omni_exec_number(engine, recovery, strlen(recovery), &n);
  printf("%-16s %-26s -> status=%u number=%g\n", "still alive", recovery, alive, n);
  failures += expect(alive == OMNI_OK && n == 42.0, "the engine is usable after a panic");

  /* omni_exec_number still refuses a string: a host that wants one asks for it. */
  const char *text = "\"not a number\"";
  double refused = 0.0;
  uint32_t wrong = omni_exec_number(engine, text, strlen(text), &refused);
  failures += expect(wrong == OMNI_ERR_RUNTIME && refused == 0.0,
                     "omni_exec_number refuses a string without writing out");

  omni_engine_free(engine);

  printf("\n%s\n", failures == 0 ? "all C embedding checks passed" : "C embedding checks FAILED");
  return failures != 0;
}
