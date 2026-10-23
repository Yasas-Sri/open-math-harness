#!/usr/bin/env python3
"""Fail when git tracks a Markdown file outside the list below.

Documentation lives in comments at the code it describes (AGENTS.md). The Markdown files
kept are the project overview and contribution guide, the agents' instructions, files a
program or workflow reads or installs, and the prompt templates.

  check_markdown.py    print each tracked .md file the list lacks; exit 1 if there is one
"""

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FILES = {
    "AGENTS.md", "CLAUDE.md", "README.md", "CONTRIBUTING.md",
    # The plugin-development service answers it to the agent.
    "program-files/plugin-development/resources/guide.md",
}
TREES = ("prompts/templates/",)


def allowed(path):
    if not path.lower().endswith(".md") or path in FILES or path.startswith(TREES):
        return True
    # memories/**/templates/**: the memory crates' prompt templates.
    parts = path.split("/")
    return parts[0] == "memories" and "templates" in parts[1:-1]


def tracked():
    out = subprocess.run(["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True).stdout
    return [name for name in out.decode().split("\0") if name]


def main():
    refused = [path for path in tracked() if not allowed(path)]
    for path in refused:
        print(f"{path}: Markdown outside tools/check_markdown.py's list; document it in a comment")
    return 1 if refused else 0


if __name__ == "__main__":
    sys.exit(main())
