#!/usr/bin/env python3
"""Documented adaptations over the strict upstream port engine."""
from __future__ import annotations

import re
import subprocess
import sys
import upstream_stack_prepare as engine
from upstream_stack_test_compat import adapt_test_helpers

original_patch = engine.apply_patch
original_port = engine.port


def adapted_patch(source: str, patch: str, filename: str) -> str:
    if filename == "crates/metal2vulkan/src/tests/mod.rs":
        additions = [line[1:] for line in patch.splitlines()
                     if line.startswith("+") and not line.startswith("+++")]
        removals = [line for line in patch.splitlines()
                    if line.startswith("-") and not line.startswith("---")]
        if additions and not removals and all(
            re.fullmatch(r"mod [A-Za-z_][A-Za-z_0-9]*;", line) for line in additions
        ):
            if any(line in source.splitlines() for line in additions):
                raise RuntimeError("Refusing duplicate test-module registration")
            engine.AUDIT.append({"file": filename,
                                 "resolution": "append independent Rust test-module declarations",
                                 "declarations": additions})
            return source.rstrip() + "\n" + "\n".join(additions) + "\n"
    return original_patch(source, patch, filename)


def transactional_port(*args, **kwargs):
    try:
        return original_port(*args, **kwargs)
    except OSError as error:
        raise RuntimeError(f"Source tree prerequisite missing: {error}") from error


engine.apply_patch = adapted_patch
engine.port = transactional_port

if __name__ == "__main__":
    try:
        engine.main()
        adapt_test_helpers(engine)
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"ASSEMBLY FAILED: {error}", file=sys.stderr)
        sys.exit(1)
