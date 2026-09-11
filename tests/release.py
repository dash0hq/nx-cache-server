"""Exercise composite-action shell bodies with local stubs. Never publish anything."""
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap

ROOT = Path(__file__).resolve().parents[1]


def script(action):
    # These two actions end with the shell block under test.
    source = (ROOT / f".github/actions/{action}/action.yml").read_text()
    return textwrap.dedent(source.rsplit("      run: |\n", 1)[1])


with tempfile.TemporaryDirectory(prefix="nx-release-test-") as directory:
    root = Path(directory)
    env = {**os.environ, "VERSION": "v0.1.0", "GITHUB_SHA": "candidate", "GITHUB_REF": "refs/heads/master",
           "GITHUB_OUTPUT": str(root / "outputs"), "RELEASE_NOTES": "quote ' and $(touch INJECTED) `touch INJECTED`\nsecond line"}
    # Shell functions shadow every external command that could publish or contact GitHub.
    stubs = '''
    gh() { if [[ "$1 $2" == "release create" ]]; then printf '%s\\n' "$@" > arguments; fi; }
    git() { [[ "${FAIL_LOOKUP:-0}" == 0 ]]; }
    '''
    result = subprocess.run(["bash", "-euo", "pipefail", "-c", textwrap.dedent(stubs) + script("create-release")], cwd=root, env=env)
    assert result.returncode == 0
    assert (root / "release-notes.txt").read_text() == env["RELEASE_NOTES"] + "\n"
    assert not (root / "INJECTED").exists()
    assert "--target\ncandidate\n" in (root / "arguments").read_text()
    (root / "arguments").unlink()
    result = subprocess.run(["bash", "-euo", "pipefail", "-c", textwrap.dedent(stubs) + script("create-release")], cwd=root, env={**env, "FAIL_LOOKUP": "1"})
    assert result.returncode != 0 and not (root / "arguments").exists()

    (root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
    stubs = '''
    git() {
      if [[ "$1 $2" == "rev-parse HEAD" ]]; then echo candidate; return; fi
      [[ "${EXISTING_TAG:-0}" == 1 ]]
    }
    '''
    for changes, valid in [({}, True), ({"VERSION": "v9.9.9"}, False),
                           ({"VERSION": "v0.1.0'; touch INJECTED; '"}, False),
                           ({"GITHUB_REF": "refs/heads/feature"}, False), ({"EXISTING_TAG": "1"}, False)]:
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", textwrap.dedent(stubs) + script("validate-version")],
                                cwd=root, env={**env, **changes}, capture_output=True)
        assert (result.returncode == 0) == valid
        assert not (root / "INJECTED").exists()
print("PASS release: notes stay data, exact candidate, failed tag lookup stops publishing, version/branch/tag guards")
