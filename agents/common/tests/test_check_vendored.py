"""check_vendored: the guard that keeps the vendored copies of shared agent modules honest."""

from __future__ import annotations

from pathlib import Path

import check_vendored
import pytest
from check_vendored import KNOWN_DIVERGENT, Problem, check

CANONICAL = "# canonical\nVALUE = 1\n"


@pytest.fixture
def agents(tmp_path: Path) -> Path:
    """An agents tree with the canonical modules and one agent that vendors the first two correctly."""
    write(tmp_path / "common" / "nasiko_state.py", CANONICAL)
    write(tmp_path / "common" / "telemetry.py", CANONICAL)
    write(tmp_path / "common" / "nasiko_adk_sessions.py", CANONICAL)
    write(tmp_path / "alpha" / "src" / "nasiko_state.py", CANONICAL)
    write(tmp_path / "alpha" / "src" / "telemetry.py", CANONICAL)
    write(
        tmp_path / "alpha" / "src" / "__main__.py",
        "from nasiko_state import make_task_store\nfrom telemetry import init\n",
    )
    return tmp_path


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def messages(problems: list[Problem]) -> str:
    return "\n".join(problem.message for problem in problems)


def test_a_tree_in_sync_has_no_problems(agents):
    assert check(agents, known_divergent={}) == []


def test_a_drifted_copy_is_reported_with_the_command_that_fixes_it(agents):
    write(agents / "alpha" / "src" / "nasiko_state.py", CANONICAL + "VALUE = 2\n")

    problems = check(agents, known_divergent={})

    assert len(problems) == 1
    assert "alpha/src/nasiko_state.py differs" in problems[0].message.replace("\\", "/")
    assert problems[0].fix.startswith("cp ") and problems[0].fix.endswith("nasiko_state.py")


def test_every_drifted_copy_is_reported_not_just_the_first(agents):
    for agent in ("beta", "gamma"):
        write(agents / agent / "nasiko_state.py", "stale\n")

    assert len(check(agents, known_divergent={})) == 2


def test_both_vendored_modules_are_checked(agents):
    write(agents / "alpha" / "src" / "telemetry.py", "stale telemetry\n")

    assert "telemetry.py differs" in messages(check(agents, known_divergent={}))


def test_an_agent_that_imports_a_module_without_shipping_it_is_reported(agents):
    write(agents / "beta" / "main.py", "from nasiko_state import make_task_store\n")

    problems = check(agents, known_divergent={})

    assert len(problems) == 1
    assert "beta/main.py imports nasiko_state" in problems[0].message.replace("\\", "/")
    assert "would not start" in problems[0].message


@pytest.mark.parametrize(
    "statement",
    ["import nasiko_state", "    from nasiko_state import make_task_store", "import nasiko_state as state"],
)
def test_every_import_spelling_is_noticed(agents, statement):
    write(agents / "beta" / "main.py", statement + "\n")

    assert "beta" in messages(check(agents, known_divergent={}))


def test_mentions_that_are_not_imports_are_ignored(agents):
    write(
        agents / "beta" / "main.py",
        '"""See nasiko_state for how this works."""\n# from nasiko_state import x\nnasiko_state = 1\n',
    )

    assert check(agents, known_divergent={}) == []


def test_a_copy_beside_the_importing_file_satisfies_the_check(agents):
    write(agents / "beta" / "main.py", "from nasiko_state import make_task_store\n")
    write(agents / "beta" / "nasiko_state.py", CANONICAL)

    assert check(agents, known_divergent={}) == []


def test_a_known_divergent_copy_may_differ(agents):
    write(agents / "alpha" / "src" / "telemetry.py", "extended\n")

    assert check(agents, known_divergent={"telemetry.py": frozenset({"alpha/src/telemetry.py"})}) == []


def test_the_exemption_covers_only_the_listed_copy(agents):
    write(agents / "alpha" / "src" / "telemetry.py", "extended\n")
    write(agents / "beta" / "telemetry.py", "also extended\n")

    problems = check(agents, known_divergent={"telemetry.py": frozenset({"alpha/src/telemetry.py"})})

    assert "beta" in messages(problems) and "alpha" not in messages(problems)


def test_an_exemption_for_a_file_that_now_matches_must_be_removed(agents):
    problems = check(agents, known_divergent={"telemetry.py": frozenset({"alpha/src/telemetry.py"})})

    assert "now matches" in messages(problems)


def test_an_exemption_for_a_file_that_is_gone_must_be_removed(agents):
    problems = check(agents, known_divergent={"telemetry.py": frozenset({"ghost/telemetry.py"})})

    assert "does not exist" in messages(problems)


def test_a_missing_canonical_file_is_reported(agents):
    (agents / "common" / "nasiko_state.py").unlink()

    assert "canonical" in messages(check(agents, known_divergent={}))


@pytest.mark.parametrize("skipped", ["node_modules", ".venv", "target", "__pycache__"])
def test_build_and_cache_directories_are_not_scanned(agents, skipped):
    write(agents / "alpha" / skipped / "nasiko_state.py", "stale\n")

    assert check(agents, known_divergent={}) == []


def test_the_canonical_directory_and_its_tests_are_not_treated_as_copies(agents):
    write(agents / "common" / "tests" / "nasiko_state.py", "a test double\n")
    write(agents / "common" / "tests" / "test_x.py", "from nasiko_state import make_task_store\n")

    assert check(agents, known_divergent={}) == []


class TestDockerfiles:
    """An agent with a Dockerfile must copy every vendored module it imports into its image."""

    def test_a_dockerfile_that_leaves_the_copy_out_is_reported(self, agents):
        write(agents / "alpha" / "Dockerfile", "FROM python:3.12-slim\nCOPY src/__main__.py /app/\n")

        problems = check(agents, known_divergent={})

        assert len(problems) == 2  # nasiko_state.py and telemetry.py
        shown = messages(problems).replace("\\", "/")
        assert "alpha/Dockerfile does not copy" in shown and "alpha/src/nasiko_state.py" in shown
        assert "would not start" in problems[0].message

    @pytest.mark.parametrize(
        "copy_line",
        [
            "COPY src/ /app",
            "COPY ./src /app",
            "COPY . .",
            "COPY src/nasiko_state.py src/telemetry.py src/__main__.py /app/",
            "COPY src/*.py /app/",
            'COPY ["src/", "/app"]',
            "COPY --chown=app:app src/ /app",
            "COPY \\\n    src/ \\\n    /app",
            "COPY alpha/src/ /app",  # built with the agents directory as its context
        ],
    )
    def test_any_copy_that_covers_the_file_satisfies_the_check(self, agents, copy_line):
        write(agents / "alpha" / "Dockerfile", f"FROM python:3.12-slim\n{copy_line}\n")

        assert check(agents, known_divergent={}) == []

    def test_a_copy_from_another_build_stage_does_not_count(self, agents):
        write(agents / "alpha" / "Dockerfile", "FROM python:3.12-slim\nCOPY --from=builder src/ /app\n")

        assert len(check(agents, known_divergent={})) == 2

    def test_each_copy_is_reported_once_however_many_files_import_it(self, agents):
        write(agents / "alpha" / "src" / "agent.py", "from nasiko_state import make_checkpointer\n")
        write(agents / "alpha" / "Dockerfile", "FROM python:3.12-slim\nCOPY src/__main__.py /app/\n")

        problems = check(agents, modules=["nasiko_state.py"], known_divergent={})

        assert len(problems) == 1

    def test_an_agent_without_a_dockerfile_is_not_checked(self, agents):
        assert not (agents / "alpha" / "Dockerfile").exists()

        assert check(agents, known_divergent={}) == []


def test_the_repository_itself_is_in_sync():
    """What CI runs, against the real tree: every vendored copy matches, none is missing."""
    problems = check(check_vendored.AGENTS_DIR)

    assert problems == [], messages(problems)


def test_the_divergent_telemetry_copies_are_the_documented_four():
    assert KNOWN_DIVERGENT["telemetry.py"] == {
        "doc-reader/telemetry.py",
        "langchain/src/telemetry.py",
        "langgraph/src/telemetry.py",
        "translator/src/telemetry.py",
    }


def test_main_exits_zero_when_in_sync_and_nonzero_otherwise(agents, monkeypatch, capsys):
    monkeypatch.setattr(check_vendored, "AGENTS_DIR", agents)
    monkeypatch.setattr(check_vendored, "KNOWN_DIVERGENT", {})
    assert check_vendored.main() == 0
    assert "match" in capsys.readouterr().out

    write(agents / "alpha" / "src" / "nasiko_state.py", "stale\n")
    assert check_vendored.main() == 1
    assert "differs" in capsys.readouterr().out


def test_the_adk_session_module_is_vendored_and_checked_like_the_others(agents):
    write(agents / "beta" / "src" / "agent.py", "from nasiko_adk_sessions import session_service_from_env\n")

    problems = check(agents, known_divergent={})

    assert len(problems) == 1
    assert "beta/src/agent.py imports nasiko_adk_sessions" in problems[0].message.replace("\\", "/")
