#!/usr/bin/env python3
"""Check ownership and dependency direction without building the CLI."""
from pathlib import Path

import tomllib  # ty: ignore[unresolved-import]  # pyright: ignore[reportMissingImports]

ROOT = Path(__file__).resolve().parents[2]
DPRINT_CRATES = {path.parent.name for path in (ROOT / "crates").glob("*/Cargo.toml")}
FORBIDDEN = {
    "dprint-git": DPRINT_CRATES - {"dprint-git"},
    "dprint-configuration": {"dprint-formatting", "dprint-plugin-types", "dprint-async-runtime"},
    "dprint-formatting": {"dprint-configuration", "dprint-plugin-types", "dprint-async-runtime"},
    "dprint-host-api": {"dprint-config", "dprint-platform", "dprint-discovery", "dprint-plugin-host", "dprint-host", "dprint-lsp"},
    "dprint-platform": {"dprint-config", "dprint-discovery", "dprint-plugin-host", "dprint-host", "dprint-lsp"},
    "dprint-discovery": {"dprint-config", "dprint-plugin-host", "dprint-host", "dprint-lsp"},
    "dprint-config": {"dprint-plugin-host", "dprint-host", "dprint-lsp"},
    "dprint-plugin-host": {"dprint-host", "dprint-lsp"},
    "dprint-host": {"dprint-lsp", "dprint-process-plugin"},
    "dprint-test-support": {"dprint-host", "dprint-lsp", "clap"},
}
errors = []
for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
    data = tomllib.loads(manifest.read_text())
    name = data["package"]["name"]
    if manifest.parent.name != name:
        errors.append(f"{manifest.relative_to(ROOT)}: directory must be named {name}")
    if name == "dprint":
        continue
    blocked = FORBIDDEN.get(name, set()) | {"dprint"}
    sections = [data, *data.get("target", {}).values()]
    for section in sections:
        for kind in ("dependencies", "build-dependencies", "dev-dependencies"):
            for alias, spec in section.get(kind, {}).items():
                package = spec.get("package", alias) if isinstance(spec, dict) else alias
                if package in blocked:
                    errors.append(f"{name}: {kind} cannot depend on {package}")
                if name == "dprint-test-support" and package in {"dprint-plugin-host", "dprint-config"}:  # ruff: ignore[collapsible-if]
                    if not isinstance(spec, dict) or not spec.get("optional"):
                        errors.append(f"{name}: {package} must be optional for lightweight fixtures")
    for source in (manifest.parent / "src").rglob("*.rs"):
        if "dprint::" in source.read_text():
            errors.append(f"{source.relative_to(ROOT)}: library must not call the CLI")
if errors:
    raise SystemExit("\n".join(errors))
print("Crate names and dependency boundaries passed.")
