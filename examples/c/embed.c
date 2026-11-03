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
 */

#include <stdio.h>
#include <string.h>

#include "omni.h"

/* Evaluate and print one program. Returns the status code. */
static uint32_t show(OmniEngine *engine, const char *label, const char *src) {
  OmniValue result;
  memset(&result, 0xFF, sizeof result); /* poison: a failure must not look like 0 */

  uint32_t code = omni_exec(engine, src, strlen(src), &result);
  if (code == OMNI_OK) {
    printf("%-14s %-22s -> tag=%u number=%g\n", label, src, result.tag, result.number);
  } else {
    char message[128];
    size_t written = omni_last_error(engine, message, sizeof message);
    printf("%-14s %-22s -> status=%u error=%s (%zu bytes)\n", label, src, code, message, written);
  }
  return code;
}

int main(void) {
  OmniEngine *engine = omni_engine_new();
  if (engine == NULL) {
    fprintf(stderr, "omni_engine_new returned NULL\n");
    return 1;
  }

  printf("OmniEngine %s\n\n", omni_version());

  /* The acceptance program: 42 must come out of the C surface unchanged. */
  uint32_t forty_two = show(engine, "vertical slice", "let answer = 40 + 2");
  show(engine, "precedence", "2 + 3 * 4");
  show(engine, "division", "20 / 3");
  show(engine, "parentheses", "(2 + 3) * 4");
  show(engine, "modulus", "20 % 7");

  /* Errors are data, not crashes. */
  uint32_t syntax = show(engine, "syntax error", "let = 1");
  uint32_t runtime = show(engine, "divide by zero", "1 / 0");

  /* The panic boundary: this returns instead of taking the process down. */
  uint32_t panic = omni_debug_panic(engine);
  printf("\n%-14s              -> status=%u (expected %u)\n", "panic boundary", panic, OMNI_ERR_PANIC);

  /* And the engine still works afterwards. */
  const char *recovery = "6 * 7";
  double n = 0.0;
  uint32_t after = omni_exec_number(engine, recovery, strlen(recovery), &n);
  printf("%-14s %-22s -> status=%u number=%g\n", "still alive", recovery, after, n);

  omni_engine_free(engine);

  int failures = 0;
  failures += (forty_two != OMNI_OK);
  failures += (syntax != OMNI_ERR_COMPILE);
  failures += (runtime != OMNI_ERR_RUNTIME);
  failures += (panic != OMNI_ERR_PANIC);
  failures += (after != OMNI_OK || n != 42.0);
  printf("\n%s\n", failures == 0 ? "all C embedding checks passed" : "C embedding checks FAILED");
  return failures;
}
