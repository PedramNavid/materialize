# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

"""Support for cross-compiling to Linux."""

import os
import platform
import sys
from enum import Enum
from typing import Dict, List

from materialize import ROOT, spawn


class Arch(Enum):
    """A CPU architecture."""

    X86_64 = "x86_64"
    """The 64-bit x86 architecture."""

    AARCH64 = "aarch64"
    """The 64-bit ARM architecture."""

    def __str__(self) -> str:
        return self.value

    def go_str(self) -> str:
        """Return the architecture name in Go nomenclature: amd64 or arm64."""
        if self == Arch.X86_64:
            return "amd64"
        elif self == Arch.AARCH64:
            return "arm64"
        else:
            raise RuntimeError("unreachable")

    @staticmethod
    def host() -> "Arch":
        if platform.machine() == "x86_64":
            return Arch.X86_64
        elif platform.machine() in ["aarch64", "arm64"]:
            return Arch.AARCH64
        else:
            raise RuntimeError(f"unknown host architecture {platform.machine()}")


# Hardcoded autoconf test results for libkrb5 about features available in the
# cross toolchain that its configure script cannot auto-detect when cross
# compiling.
KRB5_CONF_OVERRIDES = {
    "krb5_cv_attr_constructor_destructor": "yes",
    "ac_cv_func_regcomp": "yes",
    "ac_cv_printf_positional": "yes",
}


def target(arch: Arch) -> str:
    """Construct a Linux target triple for the specified architecture."""
    return f"{arch}-unknown-linux-gnu"


def cargo(arch: Arch, subcommand: str, rustflags: List[str]) -> List[str]:
    """Construct a Cargo invocation for cross compiling.

    Args:
        arch: The CPU architecture to build for.
        subcommand: The Cargo subcommand to invoke.
        rustflags: Override the flags passed to the Rust compiler. If the list
            is empty, the default flags are used.

    Returns:
        A list of arguments specifying the beginning of the command to invoke.
    """
    _target = target(arch)
    _target_env = _target.upper().replace("-", "_")

    rustflags += ["-Clink-arg=-Wl,--compress-debug-sections=zlib"]

    if sys.platform == "darwin":
        sysroot = spawn.capture([f"{_target}-cc", "-print-sysroot"]).strip()
        rustflags += [f"-L{sysroot}/lib"]
        extra_env = {
            f"CMAKE_SYSTEM_NAME": "Linux",
            f"CARGO_TARGET_{_target_env}_LINKER": f"{_target}-cc",
            f"TARGET_AR": f"{_target}-ar",
            f"TARGET_CPP": f"{_target}-cpp",
            f"TARGET_CC": f"{_target}-cc",
            f"TARGET_CXX": f"{_target}-c++",
            f"TARGET_CXXSTDLIB": "static=stdc++",
            f"TARGET_LD": f"{_target}-ld",
            f"TARGET_RANLIB": f"{_target}-ranlib",
        }
    else:
        # NOTE(benesch): The required Rust flags have to be duplicated with
        # their definitions in ci/builder/Dockerfile because `rustc` has no way
        # to merge together Rust flags from different sources.
        rustflags += [
            "-Clink-arg=-fuse-ld=lld",
            f"-L/opt/x-tools/{_target}/{_target}/sysroot/lib",
        ]
        extra_env: Dict[str, str] = {}

    env = {
        **extra_env,
        "RUSTFLAGS": " ".join(rustflags),
        **KRB5_CONF_OVERRIDES,
    }

    return [
        *_enter_builder(arch),
        "env",
        *(f"{k}={v}" for k, v in env.items()),
        "cargo",
        subcommand,
        "--target",
        _target,
    ]


def tool(arch: Arch, name: str) -> List[str]:
    """Constructs a cross-compiling binutils tool invocation.

    Args:
        arch: The CPU architecture to build for.
        name: The name of the binutils tool to invoke.

    Returns:
        A list of arguments specifying the beginning of the command to invoke.
    """
    return [
        *_enter_builder(arch),
        f"{target(arch)}-{name}",
    ]


def _enter_builder(arch: Arch) -> List[str]:
    assert (
        arch == Arch.host()
    ), f"target architecture {arch} does not match host architecture {Arch.host()}"
    if "MZ_DEV_CI_BUILDER" in os.environ:
        return []
    elif sys.platform == "darwin":
        # Building in Docker for Mac is painfully slow, so we install a
        # cross-compiling toolchain on the host and use that instead.
        _bootstrap_darwin(arch)
        return []
    else:
        return ["bin/ci-builder", "run", "stable"]


def _bootstrap_darwin(arch: Arch) -> None:
    BOOTSTRAP_VERSION = "4"
    BOOTSTRAP_FILE = ROOT / "target" / target(arch) / ".xcompile-bootstrap"
    try:
        contents = BOOTSTRAP_FILE.read_text()
    except FileNotFoundError:
        contents = ""
    if contents == BOOTSTRAP_VERSION:
        return

    # TODO(benesch): no need to clean up these old taps once sufficient time has
    # passed (say, March 2022).
    for old_tap in ["SergioBenitez/osxct", "messense/macos-cross-toolchains"]:
        spawn.runv(["brew", "uninstall", "-f", f"{old_tap}/{target(arch)}"])
    spawn.runv(["brew", "install", f"materializeinc/crosstools/{target(arch)}"])
    spawn.runv(["cargo", "clean", "--target", target(arch)], cwd=ROOT)
    spawn.runv(["rustup", "target", "add", target(arch)])

    BOOTSTRAP_FILE.parent.mkdir(parents=True, exist_ok=True)
    BOOTSTRAP_FILE.write_text(BOOTSTRAP_VERSION)
