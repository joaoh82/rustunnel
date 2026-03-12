class Rustunnel < Formula
  desc "Self-hosted secure tunnel client — expose local services through a public server"
  homepage "https://github.com/joaoh82/rustunnel"
  version "{{VERSION}}"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/joaoh82/rustunnel/releases/download/v{{VERSION}}/rustunnel-v{{VERSION}}-aarch64-apple-darwin.tar.gz"
      sha256 "{{SHA256_MACOS_ARM64}}"
    end

    on_intel do
      url "https://github.com/joaoh82/rustunnel/releases/download/v{{VERSION}}/rustunnel-v{{VERSION}}-x86_64-apple-darwin.tar.gz"
      sha256 "{{SHA256_MACOS_X86_64}}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/joaoh82/rustunnel/releases/download/v{{VERSION}}/rustunnel-v{{VERSION}}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA256_LINUX_ARM64}}"
    end

    on_intel do
      url "https://github.com/joaoh82/rustunnel/releases/download/v{{VERSION}}/rustunnel-v{{VERSION}}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA256_LINUX_X86_64}}"
    end
  end

  def install
    bin.install "rustunnel"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/rustunnel --version")
  end
end
