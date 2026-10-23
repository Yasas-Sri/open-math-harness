import contextlib
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import check_markdown


class Allowed(unittest.TestCase):
    def test_the_listed_files_and_trees_and_every_other_kind_of_file(self):
        for path in ["AGENTS.md", "CLAUDE.md", "README.md", "program-files/plugin-development/resources/guide.md",
                     "prompts/templates/compact/prompt.md", "memories/read/templates/read_path.md",
                     "memories/write/templates/consolidation.md",
                     "docs/notes.txt", "program-files/fs/src/lib.rs"]:
            self.assertTrue(check_markdown.allowed(path), path)

    def test_any_other_markdown_file(self):
        for path in ["docs/guide.md", "program-files/fs/NOTES.md", "third-party/x/README.md",
                     "compute/AGENTS.md", "docs/release-notes.md", "prompts/README.md",
                     "prompts/templates.md", "memories/templates.md", "memories/read/notes.md",
                     "skills.md", "third-party.md", "NOTES.MD", "agents.md"]:
            self.assertFalse(check_markdown.allowed(path), path)


class Main(unittest.TestCase):
    def test_lists_the_tracked_markdown_files_outside_the_list(self):
        with tempfile.TemporaryDirectory() as root:
            for name in ["README.md", "docs/new.md", "prompts/templates/a.md", "src/lib.rs"]:
                (Path(root) / name).parent.mkdir(parents=True, exist_ok=True)
                (Path(root) / name).write_text("x\n")
            subprocess.run(["git", "init", "-q", root], check=True)
            subprocess.run(["git", "-C", root, "add", "."], check=True)
            (Path(root) / "untracked.md").write_text("never committed\n")
            with patch.object(check_markdown, "ROOT", Path(root)):
                out = io.StringIO()
                with contextlib.redirect_stdout(out):
                    self.assertEqual(check_markdown.main(), 1)
                self.assertEqual(out.getvalue().split(":")[0], "docs/new.md")
                subprocess.run(["git", "-C", root, "rm", "-q", "--cached", "docs/new.md"], check=True)
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(check_markdown.main(), 0)


if __name__ == "__main__":
    unittest.main()
