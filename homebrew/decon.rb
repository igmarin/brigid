# Homebrew formula for decon.
#
# This file is a template. The `url` and `sha256` values must be updated
# for each release. The canonical live formula lives in the
# `igmarin/homebrew-tap` repository and is updated automatically (or
# manually) when a new GitHub Release is published.
#
# Usage (end users):
#   brew tap igmarin/homebrew-tap
#   brew install decon
#
# Manual update instructions:
#   1. Download the macOS archive for the new release.
#   2. Compute the SHA-256: `shasum -a 256 decon-VERSION-aarch64-apple-darwin.tar.gz`
#   3. Update `version`, `url`, and `sha256` below.
#   4. Push to the `igmarin/homebrew-tap` repository.

class Decon < Formula
  desc "Deconstruct code monoliths into structured, beginner-friendly tutorials"
  homepage "https://github.com/igmarin/decon-rs"
  license "MIT"
  version "0.1.0"

  # The formula supports both Intel and Apple Silicon Macs via `on_macos`
  # blocks. Homebrew selects the correct URL based on the arch.
  on_macos do
    on_arm do
      url "https://github.com/igmarin/decon-rs/releases/download/v#{version}/decon-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256_FOR_AARCH64"
    end
    on_intel do
      url "https://github.com/igmarin/decon-rs/releases/download/v#{version}/decon-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256_FOR_X86_64"
    end
  end

  # The archive contains: decon (binary), decon.1 (man page),
  # completions/ (bash, zsh, fish, powershell), README.md.
  def install
    bin.install "decon"
    man1.install "decon.1"
    bash_completion.install "completions/decon.bash" => "decon"
    zsh_completion.install "completions/_decon"
    fish_completion.install "completions/decon.fish"
  end

  test do
    assert_match "decon #{version}", shell_output("#{bin}/decon --version")
    assert_match "crawl", shell_output("#{bin}/decon --help")
  end
end
