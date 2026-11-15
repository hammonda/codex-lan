#!/usr/bin/env python3
"""Package already-built Linux/Windows binaries and the source tree."""
import hashlib
import pathlib
import tarfile
import zipfile

root = pathlib.Path(__file__).resolve().parents[1]
dist = root / "dist"
dist.mkdir(exist_ok=True)
linux = root / "target/portable-linux/x86_64-unknown-linux-gnu/release"
windows = root / "target/windows/x86_64-pc-windows-gnu/release"
shared = [root / "README.md", root / "LICENSE", root / "docs/VERIFICATION.md"]
shared += sorted((root / "docs/images").glob("*.png"))
artifacts = []

linux_archive = dist / "codex-lan-linux-x64.tar.gz"
with tarfile.open(linux_archive, "w:gz") as archive:
    for name in ("codex-lan-client", "codex-lan-server"):
        archive.add(linux / name, arcname=f"codex-lan/{name}")
    for file in shared:
        archive.add(file, arcname="codex-lan/" + file.relative_to(root).as_posix())
artifacts.append(linux_archive)

windows_archive = dist / "codex-lan-windows-x64.zip"
with zipfile.ZipFile(windows_archive, "w", zipfile.ZIP_DEFLATED) as archive:
    for name in ("codex-lan-client.exe", "codex-lan-server.exe"):
        archive.write(windows / name, f"codex-lan/{name}")
    for file in shared:
        archive.write(file, "codex-lan/" + file.relative_to(root).as_posix())
artifacts.append(windows_archive)

source_archive = dist / "codex-lan-source.zip"
with zipfile.ZipFile(source_archive, "w", zipfile.ZIP_DEFLATED) as archive:
    files = [
        root / name
        for name in ("Cargo.toml", "Cargo.lock", "README.md", "LICENSE", ".gitignore", ".gitattributes")
    ]
    for directory in ("crates", "docs", "tools", ".github"):
        files += [p for p in (root / directory).rglob("*") if p.is_file() and "__pycache__" not in p.parts]
    for file in sorted(files):
        archive.write(file, "codex-lan/" + file.relative_to(root).as_posix())
artifacts.append(source_archive)

checksums = []
for file in artifacts:
    digest = hashlib.sha256(file.read_bytes()).hexdigest()
    checksums.append(f"{digest}  {file.name}")
    print(f"{file.name}: {file.stat().st_size / 1024 / 1024:.1f} MiB")
(dist / "SHA256SUMS").write_text("\n".join(checksums) + "\n")
