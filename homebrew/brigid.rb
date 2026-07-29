# Homebrew formula for brigid.
#
# This formula builds brigid from source using cargo. No pre-built macOS
# binaries are shipped in GitHub Releases — only Linux x86_64. macOS users
# install via Homebrew (which compiles natively) or `cargo install brigid-cli`.
#
# This file is a template. The `sha256` value must be updated for each
# release. The canonical live formula lives in the `igmarin/homebrew-tap`
# repository and is updated when a new GitHub Release is published.
#
# Usage (end users):
#   brew tap igmarin/homebrew-tap
#   brew install brigid
#
# Manual update instructions:
#   1. Download the source tarball: brigid vVERSION
#   2. Compute the SHA-256: `shasum -a 256 brigid-VERSION.tar.gz`
#   3. Update `version`, `sha256` below.
#   4. Push to the `igmarin/homebrew-tap` repository.

class Brigid < Formula
  desc "Deconstruct code monoliths into structured, beginner-friendly tutorials"
  homepage "https://github.com/igmarin/brigid"
  license "MIT"
  version "1.1.0"

  url "https://github.com/igmarin/brigid/archive/refs/tags/v#{version}.tar.gz"
  sha256 "REPLACE_WITH_ACTUAL_SHA256_OF_SOURCE_TARBALL"

  depends_on "rust" => :build

  def install
    system "cargo", "install", "--locked", "--root", prefix, "--path", "crates/brigid-cli"
  end

  test do
    assert_match "brigid #{version}", shell_output("#{bin}/brigid --version")
    assert_match "crawl", shell_output("#{bin}/brigid --help")
  end
end
