# Homebrew cask sketch for Weave (Rust rewrite).
#
# Living draft: a cask is only installable once this repo is public or a
# token-authenticated tap exists.
# The sha256 stanzas are refreshed per release from SHA256SUMS.
cask "wv" do
  version "0.1.0"
  sha256 arm:   "<sha256 from SHA256SUMS>",
         x86_64: "<sha256 from SHA256SUMS>"

  on_arm do
    url "https://mnemosyne.sigilzero.dev/sigilzero/weave/releases/download/v#{version}/wv-#{version}-darwin-arm64.tar.gz"
  end
  on_intel do
    url "https://mnemosyne.sigilzero.dev/sigilzero/weave/releases/download/v#{version}/wv-#{version}-darwin-x64.tar.gz"
  end

  name "Weave"
  desc "RLM agent harness"
  homepage "https://mnemosyne.sigilzero.dev/sigilzero/weave"

  livecheck do
    url :url
    regex(/wv-v?(\d+(?:\.\d+)*)-darwin-arm64\.tar\.gz/i)
  end

  # `binary` symlinks into PATH; the binary resolves its package dir from the
  # real executable path (current_exe follows the symlink), so the bundled
  # prime-agent-runtime/ and skills/ next to it in the caskroom just work.
  binary "wv"

  zap trash: [
    "~/.prime", # sessions, subagents, skills, kernel venvs
  ]
end
