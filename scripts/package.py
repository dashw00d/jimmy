#!/usr/bin/env python3
"""Package a built release executable with notices, checksum and CycloneDX SBOM."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import tarfile
import zipfile


def with_cactus(path):
    bom = json.loads(path.read_text())
    ref = "vendor:cactus@4.1.4"
    bom.setdefault("components", []).append({
        "type": "library",
        "bom-ref": ref,
        "name": "Cactus runtime",
        "version": "4.1.4",
        "licenses": [{"license": {"id": "Apache-2.0"}}],
        "externalReferences": [{"type": "distribution", "url":
            "https://huggingface.co/Cactus-Compute/cactus/tree/f4ab2741203a282c6e7b89983de2b5e5b2e805a8/releases/4.1.4"}],
    })
    root = bom["metadata"]["component"]["bom-ref"]
    for dep in bom.get("dependencies", []):
        if dep["ref"] == root:
            dep.setdefault("dependsOn", []).append(ref)
            break
    return (json.dumps(bom, indent=2) + "\n").encode()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("target")
    parser.add_argument("binary", type=Path)
    parser.add_argument("sbom", type=Path)
    parser.add_argument("--out", type=Path, default=Path("dist"))
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    release = f"jimmy-{args.target}"
    root = Path(__file__).resolve().parents[1]
    files = [(args.binary, args.binary.name), (root / "THIRD_PARTY.md", "THIRD_PARTY.md"),
             (root / "licenses/cactus-4.1.4.txt", "licenses/cactus-4.1.4.txt")]
    sbom = with_cactus(args.sbom)
    if args.binary.suffix == ".exe":
        path = args.out / (release + ".zip")
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for source, name in files:
                archive.write(source, name)
            archive.writestr("sbom.cdx.json", sbom)
    else:
        path = args.out / (release + ".tar.gz")
        with tarfile.open(path, "w:gz") as archive:
            for source, name in files:
                archive.add(source, arcname=name)
            entry = tarfile.TarInfo("sbom.cdx.json")
            entry.size = len(sbom)
            archive.addfile(entry, io.BytesIO(sbom))
    with path.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    path.with_name(path.name + ".sha256").write_text(f"{checksum}  {path.name}\n")
    (args.out / (release + ".cdx.json")).write_bytes(sbom)


if __name__ == "__main__":
    main()
