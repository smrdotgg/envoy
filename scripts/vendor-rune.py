#!/usr/bin/env python3
"""Vendor the Rune crates Envoy needs into vendor/rune.

Rune's repository is large (demo videos), so Cargo git dependencies are slow and
fragile. This copies only the crates we build, strips their tests, examples and
dev-dependencies, and writes a tiny workspace manifest so `*.workspace = true`
keys keep resolving.

Envoy's own changes to Rune are kept as patches in scripts/rune-patches/ and
applied, in name order, after copying. One that no longer applies (Rune changed
there, or took the change) stops the script: drop it or redo it.
- 0002-zbus-5: Rune's D-Bus services (portals, tray) on zbus 5, the version
  AccessKit's AT-SPI bridge uses, so Linux builds carry one D-Bus stack, not two.
- 0003-windows-dx12-only: no Vulkan backend on Windows, where DX12 always has an
  adapter (WARP); Vulkan was only the fallback for a DX12 that listed none. A DX12
  that lists none fails with a message saying what to fix.
- 0004-keycaps-feature: the `--keys`/`--pointer` overlay of headless runs behind a
  `keycaps` feature (in Rune's defaults; Envoy leaves it off), so its second
  runtime isn't built into Envoy.

Usage: scripts/vendor-rune.py <path-to-rune-checkout>
"""
import pathlib
import re
import shutil
import subprocess
import sys

CRATES = ["rune", "rune-kit", "rune-motion", "rune-staging", "rune-taffy", "procwatch"]
DROP_DIRS = {"tests", "examples", "benches"}


def strip_manifest(text: str) -> str:
    out, skip = [], False
    for line in text.splitlines():
        header = re.match(r"^\s*\[\[?([^\]]+)\]\]?\s*$", line)
        if header:
            name = header.group(1)
            skip = (
                name in ("test", "example", "bench")
                or name.endswith("dev-dependencies")
            )
        if not skip:
            out.append(line)
    return "\n".join(out) + "\n"


def main() -> None:
    src = pathlib.Path(sys.argv[1]).resolve()
    dst = pathlib.Path(__file__).resolve().parent.parent / "vendor" / "rune"
    if dst.exists():
        shutil.rmtree(dst)
    (dst / "crates").mkdir(parents=True)
    for crate in CRATES:
        shutil.copytree(
            src / "crates" / crate,
            dst / "crates" / crate,
            ignore=lambda d, names: [n for n in names if n in DROP_DIRS and pathlib.Path(d).name == crate],
        )
        manifest = dst / "crates" / crate / "Cargo.toml"
        manifest.write_text(strip_manifest(manifest.read_text()))
    root = (src / "Cargo.toml").read_text()
    lints = root[root.index("[workspace.lints.rust]"):]
    members = ",\n".join(f'    "crates/{c}"' for c in CRATES)
    (dst / "Cargo.toml").write_text(
        "# Vendored subset of github.com/anomalyco/rune. Regenerate with scripts/vendor-rune.py.\n"
        f"[workspace]\nresolver = \"3\"\nmembers = [\n{members},\n]\n\n"
        "[workspace.package]\nedition = \"2024\"\nlicense = \"MIT\"\n\n" + lints
    )
    shutil.copy(src / "LICENSE", dst / "LICENSE")
    rev = subprocess.run(["git", "-C", str(src), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    (dst / "REV").write_text(rev + "\n")
    print(f"vendored rune {rev} into {dst}")
    repo = dst.parent.parent
    for patch in sorted((pathlib.Path(__file__).resolve().parent / "rune-patches").glob("*.patch")):
        subprocess.run(["git", "-C", str(repo), "apply", str(patch)], check=True)
        print(f"applied {patch.name}")


if __name__ == "__main__":
    main()
