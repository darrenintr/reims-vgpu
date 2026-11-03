#!/usr/bin/env python3
"""Candidate entrypoint and explicit adaptations for the retained fork.

The generic, fail-closed port engine lives in upstream_stack_prepare.py.
Rust test-module declaration order is not semantic: registering a new test
module at the end avoids depending on unrelated upstream test additions.
No production-code conflict is resolved by this special case.
"""
from __future__ import annotations

import re
import subprocess
import sys
import upstream_stack_prepare as engine

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
        # Missing intermediate-upstream files are a failed port, not permission
        # to transplant a partial feature. The engine records optional deferrals.
        raise RuntimeError(f"Source tree prerequisite missing: {error}") from error


engine.apply_patch = adapted_patch
engine.port = transactional_port

if __name__ == "__main__":
    try:
        engine.main()
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"ASSEMBLY FAILED: {error}", file=sys.stderr)
        sys.exit(1)
