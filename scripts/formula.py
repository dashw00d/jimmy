#!/usr/bin/env python3
"""Generate the Homebrew formula from a GitHub release artifact directory."""
import argparse
from pathlib import Path
import re


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("repository", help="GitHub owner/repository")
    cli.add_argument("tag")
    cli.add_argument("artifacts", type=Path)
    args = cli.parse_args()
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repository):
        cli.error("repository must be owner/repository")
    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", args.tag):
        cli.error("tag must be a v-prefixed release version")
    base = f"https://github.com/{args.repository}"
    print(f'''class Jimmy < Formula
  desc "Local speech recognition, synthesis, and wake detection"
  homepage "{base}"
  version "{args.tag[1:]}"

  on_macos do''')
    for ruby_name, arch in [("arm", "aarch64"), ("intel", "x86_64")]:
        name = f"jimmy-{arch}-apple-darwin.tar.gz"
        checksum = (args.artifacts / f"{name}.sha256").read_text().split()[0]
        if not re.fullmatch("[a-f0-9]{64}", checksum):
            cli.error(f"invalid checksum for {name}")
        print(f'''    on_{ruby_name} do
      url "{base}/releases/download/{args.tag}/{name}"
      sha256 "{checksum}"
    end''')
    print('''  end

  def install
    bin.install "jimmy"
  end

  test do
    assert_match "jimmy", shell_output("#{bin}/jimmy --version")
  end
end''')


if __name__ == "__main__":
    main()
