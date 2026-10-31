#!/usr/bin/env python3
"""Fail when a vendored agent module has drifted from its canonical copy, or is missing.

`nasiko push` and `nasiko deploy` build an agent's image from the agent's own directory only, so
an agent cannot import a module that lives anywhere else. Shared modules therefore live once in
oss/agents/common (canonical) and are copied next to each agent's main module. This script keeps
those copies honest:

  - a copy that differs from the canonical file fails the check;
  - an agent source file that imports a vendored module, with no copy beside it, fails the check
    (the image would crash on startup with ImportError);
  - so does an agent whose Dockerfile builds the image without copying that copy in;
  - an entry in KNOWN_DIVERGENT that no longer needs to be there fails the check, so the list can
    only shrink.

Standard library only, so CI runs it without installing anything:

    python3 oss/agents/common/check_vendored.py
"""

from __future__ import annotations

import fnmatch
import json
import os
import re
import sys
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from pathlib import Path

AGENTS_DIR = Path(__file__).resolve().parent.parent
COMMON_DIR_NAME = "common"
VENDORED_MODULES = ("telemetry.py", "nasiko_state.py", "nasiko_adk_sessions.py")
SKIPPED_DIRECTORIES = frozenset({"node_modules", "target", ".venv", "__pycache__", ".git"})

# Copies that predate this check and carry agent-specific additions on top of the canonical file
# (a TraceparentMiddleware, an extra instrumentor). They are exempt from the equality check, and
# only from that. Reconcile one with the canonical file, then delete its line.
KNOWN_DIVERGENT: Mapping[str, frozenset[str]] = {
    "telemetry.py": frozenset(
        {
            "doc-reader/telemetry.py",
            "langchain/src/telemetry.py",
            "langgraph/src/telemetry.py",
            "translator/src/telemetry.py",
        }
    ),
}


@dataclass(frozen=True)
class Problem:
    message: str
    fix: str | None = None


def check(
    agents_dir: Path,
    modules: Iterable[str] = VENDORED_MODULES,
    known_divergent: Mapping[str, frozenset[str]] = KNOWN_DIVERGENT,
) -> list[Problem]:
    problems: list[Problem] = []
    for module in modules:
        canonical = agents_dir / COMMON_DIR_NAME / module
        if not canonical.is_file():
            problems.append(Problem(f"the canonical file {_shown(canonical)} is missing"))
            continue
        exempt = known_divergent.get(module, frozenset())
        problems += _drifted_copies(agents_dir, canonical, exempt)
        problems += _stale_exemptions(agents_dir, canonical, exempt)
        importers = _importers(agents_dir, module)
        problems += _imports_without_copy(agents_dir, module, importers)
        problems += _copies_left_out_of_images(agents_dir, module, importers)
    return problems


def _drifted_copies(agents_dir: Path, canonical: Path, exempt: frozenset[str]) -> list[Problem]:
    wanted = canonical.read_bytes()
    problems = []
    for copy in _copies(agents_dir, canonical.name):
        if _relative(agents_dir, copy) in exempt or copy.read_bytes() == wanted:
            continue
        problems.append(
            Problem(
                f"{_shown(copy)} differs from {_shown(canonical)}",
                fix=f"cp {_shown(canonical)} {_shown(copy)}",
            )
        )
    return problems


def _stale_exemptions(agents_dir: Path, canonical: Path, exempt: frozenset[str]) -> list[Problem]:
    wanted = canonical.read_bytes()
    problems = []
    for relative in sorted(exempt):
        copy = agents_dir / relative
        if not copy.is_file():
            problems.append(
                Problem(f"KNOWN_DIVERGENT lists {relative}, which does not exist; remove the entry")
            )
        elif copy.read_bytes() == wanted:
            problems.append(Problem(f"KNOWN_DIVERGENT lists {relative}, which now matches; remove the entry"))
    return problems


def _importers(agents_dir: Path, module: str) -> list[Path]:
    """Every agent source file that imports the module."""
    name = module.removesuffix(".py")
    imports = re.compile(rf"^\s*(?:from\s+{name}\s+import|import\s+{name}\b)", re.MULTILINE)
    return [source for source in _python_sources(agents_dir) if _imports(source, imports)]


def _imports_without_copy(agents_dir: Path, module: str, importers: list[Path]) -> list[Problem]:
    canonical = agents_dir / COMMON_DIR_NAME / module
    problems = []
    for source in importers:
        if (source.parent / module).is_file():
            continue
        problems.append(
            Problem(
                f"{_shown(source)} imports {module.removesuffix('.py')} but {module} is not beside it; "
                "the image would not start",
                fix=f"cp {_shown(canonical)} {_shown(source.parent / module)}",
            )
        )
    return problems


def _copies_left_out_of_images(agents_dir: Path, module: str, importers: list[Path]) -> list[Problem]:
    """Copies an agent imports that its Dockerfile never puts into the image."""
    problems = []
    checked: set[Path] = set()
    for source in importers:
        copy = source.parent / module
        dockerfile = _dockerfile_of(agents_dir, source)
        if copy in checked or not copy.is_file() or dockerfile is None:
            continue
        checked.add(copy)
        if _dockerfile_copies(dockerfile, agents_dir, copy):
            continue
        problems.append(
            Problem(
                f"{_shown(dockerfile)} does not copy {_shown(copy)} into the image; the image would not start",
                fix=f"add a COPY of {_relative(dockerfile.parent, copy)} to {_shown(dockerfile)}",
            )
        )
    return problems


def _dockerfile_of(agents_dir: Path, source: Path) -> Path | None:
    """The Dockerfile at the root of the agent the source belongs to, if it has one."""
    agent_dir = agents_dir / source.relative_to(agents_dir).parts[0]
    dockerfile = agent_dir / "Dockerfile"
    return dockerfile if dockerfile.is_file() else None


def _dockerfile_copies(dockerfile: Path, agents_dir: Path, copy: Path) -> bool:
    # The build context is the agent's directory, or (for a few agents) the agents directory above it.
    candidates = {_relative(dockerfile.parent, copy), _relative(agents_dir, copy)}
    return any(_covers(source, path) for source in _copied_sources(dockerfile) for path in candidates)


def _copied_sources(dockerfile: Path) -> list[str]:
    """The source paths of every COPY or ADD that takes files from the build context."""
    text = dockerfile.read_text(encoding="utf-8", errors="replace").replace("\\\n", " ")
    sources: list[str] = []
    for line in text.splitlines():
        instruction, _, rest = line.strip().partition(" ")
        if instruction.upper() not in ("COPY", "ADD"):
            continue
        words = rest.split()
        if any(word.startswith("--from") for word in words):
            continue  # copies out of another build stage, not out of the build context
        arguments = " ".join(word for word in words if not word.startswith("--"))
        paths = json.loads(arguments) if arguments.startswith("[") else arguments.split()
        sources += paths[:-1]  # the last path is the destination
    return sources


def _covers(source: str, path: str) -> bool:
    source = source.removeprefix("./").rstrip("/")
    if source in ("", "."):
        return True
    return path == source or path.startswith(source + "/") or fnmatch.fnmatchcase(path, source)


def _imports(source: Path, pattern: re.Pattern[str]) -> bool:
    return pattern.search(source.read_text(encoding="utf-8", errors="replace")) is not None


def _copies(agents_dir: Path, module: str) -> list[Path]:
    return [path for path in _walk(agents_dir) if path.name == module]


def _python_sources(agents_dir: Path) -> list[Path]:
    return [path for path in _walk(agents_dir) if path.suffix == ".py"]


def _walk(agents_dir: Path) -> list[Path]:
    """Every file under the agents, minus the canonical directory and build or cache output."""
    files = []
    for directory, subdirectories, names in os.walk(agents_dir):
        subdirectories[:] = sorted(d for d in subdirectories if d not in SKIPPED_DIRECTORIES)
        if Path(directory) == agents_dir:
            subdirectories[:] = [d for d in subdirectories if d != COMMON_DIR_NAME]
        files += [Path(directory) / name for name in sorted(names)]
    return files


def _relative(agents_dir: Path, path: Path) -> str:
    return path.relative_to(agents_dir).as_posix()


def _shown(path: Path) -> str:
    """The path as the developer can paste it: relative to where they ran the script."""
    try:
        return os.path.relpath(path)
    except ValueError:  # a different drive on Windows
        return str(path)


def main() -> int:
    problems = check(AGENTS_DIR, VENDORED_MODULES, KNOWN_DIVERGENT)
    for problem in problems:
        print(f"error: {problem.message}")
        if problem.fix:
            print(f"       fix: {problem.fix}")
    if problems:
        print(f"{len(problems)} vendoring problem(s)", file=sys.stderr)
        return 1
    print("vendored agent modules match their canonical copies")
    return 0


if __name__ == "__main__":
    sys.exit(main())
