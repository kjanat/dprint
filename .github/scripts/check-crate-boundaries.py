#!/usr/bin/env python3
"""Check ownership and dependency direction without building the CLI."""
from pathlib import Path

import tomllib  # ty: ignore[unresolved-import]  # pyright: ignore[reportMissingImports]

ROOT = Path(__file__).resolve().parents[2]
KPRINT_CRATES = {path.parent.name for path in (ROOT / "crates").glob("*/Cargo.toml")}
FORBIDDEN = {
    "kprint-git": KPRINT_CRATES - {"kprint-git"},
    "kprint-configuration": {"kprint-formatting", "kprint-plugin-types", "kprint-async-runtime"},
    "kprint-formatting": {"kprint-configuration", "kprint-plugin-types", "kprint-async-runtime"},
    "kprint-host-api": {"kprint-config", "kprint-platform", "kprint-discovery", "kprint-plugin-host", "kprint-host", "kprint-lsp"},
    "kprint-platform": {"kprint-config", "kprint-discovery", "kprint-plugin-host", "kprint-host", "kprint-lsp"},
    "kprint-discovery": {"kprint-config", "kprint-plugin-host", "kprint-host", "kprint-lsp"},
    "kprint-config": {"kprint-plugin-host", "kprint-host", "kprint-lsp"},
    "kprint-plugin-host": {"kprint-host", "kprint-lsp"},
    "kprint-host": {"kprint-lsp", "kprint-process-plugin"},
    "kprint-test-support": {"kprint-host", "kprint-lsp", "clap"},
}
errors = []
for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
    data = tomllib.loads(manifest.read_text())
    name = data["package"]["name"]
    if manifest.parent.name != name:
        errors.append(f"{manifest.relative_to(ROOT)}: directory must be named {name}")
    if name == "kprint":
        continue
    blocked = FORBIDDEN.get(name, set()) | {"kprint"}
    sections = [data, *data.get("target", {}).values()]
    for section in sections:
        for kind in ("dependencies", "build-dependencies", "dev-dependencies"):
            for alias, spec in section.get(kind, {}).items():
                package = spec.get("package", alias) if isinstance(spec, dict) else alias
                if package in blocked:
                    errors.append(f"{name}: {kind} cannot depend on {package}")
                if name == "kprint-test-support" and package in {"kprint-plugin-host", "kprint-config"}:  # ruff: ignore[collapsible-if]
                    if not isinstance(spec, dict) or not spec.get("optional"):
                        errors.append(f"{name}: {package} must be optional for lightweight fixtures")
    for source in (manifest.parent / "src").rglob("*.rs"):
        if "kprint::" in source.read_text():
            errors.append(f"{source.relative_to(ROOT)}: library must not call the CLI")
if errors:
    raise SystemExit("\n".join(errors))
print("Crate names and dependency boundaries passed.")
