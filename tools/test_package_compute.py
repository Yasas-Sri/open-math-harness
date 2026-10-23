import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import package_compute as package


class ComputePackage(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.bin = self.root / "build"
        self.out = self.root / "out"
        self.bin.mkdir()
        self.binary = bytearray(64)
        self.binary[:6] = b"\x7fELF\x02\x01"
        self.binary[18:20] = (62).to_bytes(2, "little")
        for name in package.BINARIES:
            (self.bin / name).write_bytes(self.binary)
        (self.bin / "private-key").write_text("never package arbitrary build files")
        self.write("protocol/src/message.rs", "pub const PROTOCOL_VERSION: u32 = 4;")
        source_root = Path(__file__).resolve().parents[1]
        for name in (*package.DEPLOY, package.GUIDE, *package.NOTICES):
            self.write(name, (source_root / name).read_text())
        self.shared = {"share/prompts/putnam.j2", "share/curricula/heldout.txt", "share/policies/pi_doubling.py"}
        for name in self.shared:
            kind, rest = name.removeprefix("share/").split("/", 1)
            self.write(f"{package.SHARE[kind]}/{rest}", "shared data fixture\n")
        self.addCleanup(patch.stopall)
        patch.object(package, "ROOT", self.root).start()
        self.git = patch.object(package, "git", side_effect=lambda *args: "a" * 40 if args[0] == "rev-parse" else "").start()

    def write(self, name, text):
        file = self.root / name
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)

    def test_package_holds_the_vm_composition_and_its_manifest_hashes(self):
        # The host may have no Cargo or Rust installation. Git metadata is stubbed
        # above; every other subprocess would be an unintended generation step.
        with patch.object(package.subprocess, "check_output", side_effect=AssertionError("unexpected build tool")):
            artifact, sidecar = package.build(self.bin, self.out, "0.10.3")
        self.assertEqual(artifact.name, "Open-Math-OS-0.10.0-compute-linux-x64.tar.gz")
        manifest = json.loads(sidecar.read_text())
        with tarfile.open(artifact) as archive:
            names = set(archive.getnames())
            self.assertEqual(json.loads(archive.extractfile("manifest.json").read()), manifest)
            for name, sha in manifest["files_sha256"].items():
                self.assertEqual(package.digest(archive.extractfile(name).read()), sha)
        self.assertEqual(names, set(manifest["files_sha256"]) | {"manifest.json"})
        self.assertIn("LICENSE", names)
        self.assertLessEqual({f"bin/{name}" for name in package.BINARIES} | set(package.DEPLOY) | set(package.NOTICES) | self.shared,
                             names)
        self.assertEqual((manifest["version"], manifest["ipc_version"]), ("0.10.0", 4))
        with tarfile.open(artifact) as archive:
            self.assertEqual(archive.getmember("bin/nucleus-shell").mode, 0o755)
            self.assertEqual(archive.getmember("deploy/vm/activate").mode, 0o644)  # run as `/bin/sh activate`
            self.assertEqual(archive.getmember("share/policies/pi_doubling.py").mode, 0o644)
            self.assertNotIn("private-key", archive.getnames())
        before = artifact.read_bytes()
        with self.assertRaises(FileExistsError):
            package.build(self.bin, self.out, "0.10.3")
        self.assertEqual(artifact.read_bytes(), before)

    def test_shared_data_rejects_links(self):
        (self.root / package.SHARE["policies"] / "linked.py").symlink_to("/etc/passwd")
        with self.assertRaisesRegex(ValueError, "ordinary input"):
            package.build(self.bin, self.out, "0.10.3")
        self.assertFalse(self.out.exists())

    def test_inventory_covers_the_vm_composition(self):
        source_root = Path(__file__).resolve().parents[1]
        config = json.loads((source_root / "deploy/vm/nucleus.json").read_text())
        needed = {Path(program["executable"]).name for program in config["programs"].values()}
        # Executables a program starts from its own namespace (the projects service's hosts).
        needed.update(Path(bind["host"]).name for program in config["programs"].values()
                      for bind in program["binds"] if bind["host"].startswith("/opt/nucleus/bin/"))
        args = config["programs"]["discovery"]["args"]
        needed.add(Path(args[args.index("--prover-executable") + 1]).name)
        needed.update(["nucleus", "nucleus-mcp"])
        needed.add(Path(config["identities"]["helper"]).name)
        self.assertEqual(set(package.BINARIES), needed)
        shared = {bind["host"] for program in config["programs"].values() for bind in program["binds"]
                  if bind["host"].startswith("/opt/nucleus/share/")}
        self.assertEqual(shared, {f"/opt/nucleus/share/{kind}" for kind in package.SHARE})
        for bad, message in [({"executable": "/usr/bin/other"}, "not in the package"),
                             ({"executable": "/opt/nucleus/bin/nucleus-fs",
                               "binds": [{"host": "/opt/nucleus/share/problems", "guest": "/p"}]}, "shared data")]:
            with self.subTest(message=message):
                self.write("deploy/vm/nucleus.json", json.dumps({**config, "programs": {**config["programs"], "extra": bad}}))
                with self.assertRaisesRegex(ValueError, message):
                    package.build(self.bin, self.out, "0.10.3")

    def test_failed_input_never_creates_output(self):
        for bad in [b"not an ELF", bytes(self.binary[:18]) + b"\xb7\0" + bytes(self.binary[20:])]:
            with self.subTest(binary=bad[:8]):
                (self.bin / "nucleus-fs").write_bytes(bad)
                with self.assertRaisesRegex(ValueError, "Linux x64"):
                    package.build(self.bin, self.out, "0.10.3")
                self.assertFalse(self.out.exists())
        (self.bin / "nucleus-fs").unlink()
        with self.assertRaisesRegex(ValueError, "ordinary input"):
            package.build(self.bin, self.out, "0.10.3")
        self.assertFalse(self.out.exists())


if __name__ == "__main__":
    unittest.main()
