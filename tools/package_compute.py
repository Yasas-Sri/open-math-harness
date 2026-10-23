#!/usr/bin/env python3
"""Package the offline Linux x64 VM runtime (the whole Nucleus); never install or launch it.

The archive, Open-Math-OS-X.Y.0-compute-linux-x64.tar.gz, holds the files the constants
below name and a manifest.json, written beside it too as runtime-compute-linux-x64.json,
which records the version, the IPC version, the source commit and every file's SHA-256.
Open Math OS's release workflow packages it for an X.Y.0 release (`--version`, the app's
X.Y.Z), and the release's SHA256SUMS covers both files; its deploy workflow packages it when
it installs a branch's HEAD (nightly, test). Both build the binaries in its release image,
so a VM's glibc must be at least that image's.

It is not a VM image. The image provides Lean (/opt/lean), Mathlib (/opt/mathlib), CSLib
(/opt/cslib), the mathematical Python environment (/opt/math), the problems and the Lean
REPL (/opt/nucleus/problems, /opt/nucleus/repl) and the packages the image builder installs
(open-math-os compute/images/vm.json), and the Mathlib reference index (/opt/nucleus/docs,
open-math-os tools/mathlib/install_docs.sh). A program of the composition whose
bind source is missing does not start (platform/src/confinement.rs).
"""

import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import stat
import struct
import subprocess
import tarfile


ROOT = Path(__file__).resolve().parent.parent
BINARIES = (
    "nucleus", "nucleus-conversation", "nucleus-openrouter", "nucleus-fs",
    "nucleus-memory", "nucleus-approvals", "nucleus-web", "nucleus-pdf", "nucleus-plugin-development",
    "nucleus-lean", "nucleus-shell", "nucleus-discovery", "nucleus-prover",
    "nucleus-chat-agent", "nucleus-mcp", "nucleus-ui", "nucleus-suggestions", "nucleus-diffs", "nucleus-as",
    "nucleus-projects", "nucleus-project", "nucleus-publish", "nucleus-skills",
)
# The VM composition, its units and `activate`, which the platform runs as root on every
# runtime_activate to install them (open-math-os compute/src/runtime_manage.rs).
DEPLOY = ("deploy/vm/activate", "deploy/vm/nucleus.json", "deploy/vm/nucleus.service",
          "deploy/vm/approvals.json", "deploy/vm/nucleus-as.conf")
GUIDE = "program-files/plugin-development/resources/guide.md"
# Static license texts and attributions shipped with the runtime.
NOTICES = ("LICENSE", "third-party/NOTICES.txt")
# Repo-owned read-only data of the VM composition, shipped so no image has to provide it;
# `activate` links `/opt/nucleus/share` to the bundle's `share/`.
SHARE = {"prompts": "program-files/prover/prompts", "curricula": "program-files/discovery/curricula",
         "policies": "program-files/discovery/policies"}
EXECUTABLE_FILES = {f"bin/{name}" for name in BINARIES}
MAX_ARCHIVE = 256 * 1024 * 1024
MAX_EXPANDED = 1024 * 1024 * 1024
MAX_MEMBERS = 256
# The KVM nodes' guests are x64 (ELF e_machine 62).
ARCH = "x64"
ELF_MACHINE = 62


def names(version):
    return f"Open-Math-OS-{version}-compute-linux-{ARCH}.tar.gz", f"runtime-compute-linux-{ARCH}.json"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT).decode().strip()


def release_version(app_version):
    """The runtime an app of `app_version` X.Y.Z requests: X.Y.0."""
    parts = app_version.split(".")
    if len(parts) != 3 or not all(part.isdigit() for part in parts):
        raise ValueError(f"not an X.Y.Z version: {app_version!r}")
    return f"{parts[0]}.{parts[1]}.0"


def ipc_version():
    source = (ROOT / "protocol/src/message.rs").read_text()
    return int(re.search(r"pub const PROTOCOL_VERSION:\s*u32\s*=\s*(\d+);", source)[1])


def check_binary(data, name):
    if (len(data) < 64 or data[:6] != b"\x7fELF\x02\x01"
            or struct.unpack_from("<H", data, 18)[0] != ELF_MACHINE):
        raise ValueError(f"not a Linux {ARCH} executable: {name}")


def ordinary_file(file):
    if linked(file) or not file.is_file():
        raise ValueError(f"expected ordinary input file: {file}")
    return file.read_bytes()


def linked(file):
    # Windows directory junctions are reparse points, not pathlib symlinks.
    try:
        info = file.lstat()
    except FileNotFoundError:
        return False
    return stat.S_ISLNK(info.st_mode) or bool(getattr(info, "st_file_attributes", 0) & 0x400)


def portable_path(name):
    return (isinstance(name, str) and 0 < len(name) <= 1024
            and all(re.fullmatch(r"[A-Za-z0-9_.-]+", part)
                    and part not in (".", "..") and not part.endswith(".")
                    and not re.match(r"(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)", part, re.I)
                    for part in name.split("/")))


def share_resource(name):
    parts = name.split("/")
    return portable_path(name) and len(parts) > 2 and parts[0] == "share" and parts[1] in SHARE


def share_files():
    files = {}

    def visit(directory, prefix):
        if linked(directory) or not directory.is_dir():
            raise ValueError(f"expected ordinary shared data directory: {directory}")
        for entry in sorted(directory.iterdir()):
            name = f"{prefix}/{entry.name}"
            if entry.is_dir() and not linked(entry):
                visit(entry, name)
            elif not share_resource(name):
                raise ValueError(f"unexpected shared data file: {name}")
            else:
                files[name] = ordinary_file(entry)
    for kind, source in SHARE.items():
        visit(ROOT / source, f"share/{kind}")
    check_share(files)
    return files


def check_share(names):
    if any(not any(name.startswith(f"share/{kind}/") for name in names) for kind in SHARE):
        raise ValueError("incomplete shared data")


def check_composition(data):
    """Every program of the VM composition must be a packaged binary."""
    config = json.loads(data)
    for name, program in config["programs"].items():
        path = PurePosixPath(program["executable"])
        if path.parent != PurePosixPath("/opt/nucleus/bin") or path.name not in BINARIES:
            raise ValueError(f"VM composition program is not in the package: {name}")
        for bind in program.get("binds", []):
            host = PurePosixPath(bind["host"])
            if (host.is_relative_to("/opt/nucleus/share")
                    and (len(host.parts) != 5 or host.parts[4] not in SHARE)):
                raise ValueError(f"VM composition binds shared data the package lacks: {host}")


def build(bin_dir, output_dir, app_version):
    version = release_version(app_version)
    files = {f"bin/{name}": ordinary_file(bin_dir / name) for name in BINARIES}
    for name, data in files.items():
        check_binary(data, name)
    for name in (*DEPLOY, GUIDE, *NOTICES):
        files[name] = ordinary_file(ROOT / name)
    check_composition(files["deploy/vm/nucleus.json"])
    files.update(share_files())
    if (len(files) + 1 > MAX_MEMBERS or sum(map(len, files.values())) > MAX_EXPANDED
            or len({name.lower() for name in files}) != len(files)):
        raise ValueError("compute package count, size or case collision")
    for name, data in files.items():
        if len(data) > (MAX_ARCHIVE if name in EXECUTABLE_FILES else 1024 * 1024):
            raise ValueError(f"oversized compute package input: {name}")
    manifest = {
        "v": 3, "kind": "nucleus-compute", "version": version,
        "platform": "linux", "arch": ARCH, "ipc_version": ipc_version(),
        "source_commit": git("rev-parse", "HEAD"),
        "source_dirty": bool(git("status", "--porcelain", "--untracked-files=normal")),
        "executable_files": sorted(EXECUTABLE_FILES),
        "files_sha256": {name: digest(data) for name, data in sorted(files.items())},
    }
    encoded = (json.dumps(manifest, indent=2) + "\n").encode()
    if len(encoded) > 1024 * 1024 or sum(map(len, files.values())) + len(encoded) > MAX_EXPANDED:
        raise ValueError("compute manifest or expanded package exceeds its limit")
    files["manifest.json"] = encoded
    artifact, sidecar = (output_dir / name for name in names(version))
    if artifact.exists() or sidecar.exists():
        raise FileExistsError("refusing to overwrite a compute release artifact")
    output_dir.mkdir(parents=True, exist_ok=True)
    # All inputs are validated before creating output. Exclusive writes preserve
    # an existing release; a partial output is removed if this build fails.
    sidecar_created = False
    with artifact.open("xb") as destination:
        try:
            with gzip.GzipFile(fileobj=destination, mode="wb", filename="", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as archive:
                    for name, data in sorted(files.items()):
                        entry = tarfile.TarInfo(name)
                        entry.size = len(data)
                        entry.mode = 0o755 if name in EXECUTABLE_FILES else 0o644
                        archive.addfile(entry, io.BytesIO(data))
            if destination.tell() > MAX_ARCHIVE:
                raise ValueError("compute archive exceeds its size limit")
            with sidecar.open("xb") as output:
                sidecar_created = True
                output.write(encoded)
        except BaseException:
            artifact.unlink(missing_ok=True)
            if sidecar_created:
                sidecar.unlink(missing_ok=True)
            raise
    return artifact, sidecar


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--version", required=True, help="the release's app version, X.Y.Z")
    args = parser.parse_args()
    artifact, sidecar = build(args.bin_dir, args.output_dir, args.version)
    print(f"Packaged {artifact}; manifest {sidecar}")

if __name__ == "__main__":
    main()
