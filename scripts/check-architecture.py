#!/usr/bin/env python3
"""Check production crate boundaries without building or downloading dependencies."""
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Direct production dependencies. Test adapters are deliberately excluded.
ALLOWED = {
    "oracle-ai": {"oracle-core", "oracle-contracts", "oracle-task-scope"},
    "oracle-contracts": set(),
    "oracle-task-scope": set(),
    "oracle-rpc": set(),
    "oracle-core": {"oracle-contracts", "oracle-task-scope"},
    "oracle-process": {"oracle-rpc"},
    "oracle-module-sdk": {"oracle-contracts", "oracle-rpc", "oracle-task-scope"},
    "oracle-modules": {"oracle-core", "oracle-contracts", "oracle-process", "oracle-rpc", "oracle-task-scope"},
    "oracle-operations": {"oracle-core", "oracle-contracts", "oracle-task-scope"},
    "oracle-storage": {"oracle-core", "oracle-contracts"},
    "oracle-discord": {"oracle-core", "oracle-contracts", "oracle-modules", "oracle-operations"},
    "oracle": {"oracle-ai", "oracle-core", "oracle-contracts", "oracle-storage", "oracle-discord", "oracle-modules", "oracle-operations", "oracle-task-scope"},
}
# These boundaries must also stay free of concrete third-party adapters.
ADAPTERS = {"serenity", "sqlx", "rusqlite", "tokio-postgres", "postgres", "diesel"}
PURE = {"oracle-ai", "oracle-contracts", "oracle-core", "oracle-module-sdk", "oracle-rpc", "oracle-task-scope", "oracle-process", "oracle-modules", "oracle-operations"}


def dependencies(manifest, workspace):
    """Include renamed, workspace-inherited, target-specific and build dependencies."""
    tables = [manifest, *manifest.get("target", {}).values()]
    for table in tables:
        for section in ("dependencies", "build-dependencies"):
            for alias, value in table.get(section, {}).items():
                detail = value if isinstance(value, dict) else {}
                if detail.get("workspace"):
                    inherited = workspace.get(alias, {})
                    detail = inherited if isinstance(inherited, dict) else {}
                yield detail.get("package", alias)


def violations(manifests, workspace):
    errors = []
    names = {manifest["package"]["name"] for manifest in manifests}
    for manifest in manifests:
        name = manifest["package"]["name"]
        if name not in ALLOWED:
            errors.append(f"{name}: document its production boundary in ALLOWED")
            continue
        for dependency in dependencies(manifest, workspace):
            if dependency in names or dependency.startswith("oracle-"):
                if dependency not in ALLOWED[name]:
                    errors.append(f"{name}: forbidden production dependency on {dependency}")
            if name in PURE and dependency in ADAPTERS:
                errors.append(f"{name}: concrete adapter {dependency} belongs behind a host port")
    return sorted(set(errors))


def main():
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"].get("dependencies", {})
    manifests = [tomllib.loads(path.read_text()) for path in sorted((ROOT / "crates").glob("*/Cargo.toml"))]
    errors = violations(manifests, workspace)
    for error in errors:
        print(error, file=sys.stderr)
    if not errors:
        print(f"Architecture boundaries passed for {len(manifests)} production crates")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
