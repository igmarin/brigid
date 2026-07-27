# Homebrew formula for brigid.
#
# This file is a template. The `url` and `sha256` values must be updated
# for each release. The canonical live formula lives in the
# `igmarin/homebrew-tap` repository and is updated automatically (or
# manually) when a new GitHub Release is published.
#
# Usage (end users):
#   brew tap igmarin/homebrew-tap
#   brew install brigid
#
# Manual update instructions:
#   1. Download the macOS archive for the new release.
#   2. Compute the SHA-256: `shasum -a 256 brigid-VERSION-aarch64-apple-darwin.tar.gz`
#   3. Update `version`, `url`, and `sha256` below.
#   4. Push to the `igmarin/homebrew-tap` repository.

class Brigid < Formula
  desc "Deconstruct code monoliths into structured, beginner-friendly tutorials"
  homepage "https://github.com/igmarin/brigid"
  license "MIT"
  version "1.0.0"

  # The formula supports both Intel and Apple Silicon Macs via `on_macos`
  # blocks. Homebrew selects the correct URL based on the arch.
  on_macos do
    on_arm do
      url "https://github.com/igmarin/brigid/releases/download/v#{version}/brigid-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256_FOR_AARCH64"
    end
    on_intel do
      url "https://github.com/igmarin/brigid/releases/download/v#{version}/brigid-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256_FOR_X86_64"
    end
  end

  # The archive contains: brigid (binary), brigid.1 (man page),
  # completions/ (bash, zsh, fish, powershell), README.md.
  def install
    bin.install "brigid"
    man1.install "brigid.1"
    bash_completion.install "completions/brigid.bash" => "brigid"
    zsh_completion.install "completions/_brigid"
    fish_completion.install "completions/brigid.fish"
  end

  test do
    assert_match "brigid #{version}", shell_output("#{bin}/brigid --version")
    assert_match "crawl", shell_output("#{bin}/brigid --help")
  end
end
