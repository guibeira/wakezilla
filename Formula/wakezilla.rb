class Wakezilla < Formula
  desc "Wake-on-LAN proxy server written in Rust"
  homepage "https://github.com/guibeira/wakezilla"
  version "0.2.14"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/guibeira/wakezilla/releases/download/v0.2.14/wakezilla-0.2.14-aarch64-apple-darwin.tar.gz"
      sha256 "cc8c47393ea1108dc3cc060c9a0d21e746bbc39a4ab2008acb46732f5c3ab4d6"
    else
      url "https://github.com/guibeira/wakezilla/releases/download/v0.2.14/wakezilla-0.2.14-x86_64-apple-darwin.tar.gz"
      sha256 "8859347b782a040ef53d50d5f5fa07e09128caa5adbc41ea19571f6f2d26d551"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/guibeira/wakezilla/releases/download/v0.2.14/wakezilla-0.2.14-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0a707563e2bc6c6ac8ec78887deb977da829637e8437ddbc0a01d8308bd05819"
    else
      url "https://github.com/guibeira/wakezilla/releases/download/v0.2.14/wakezilla-0.2.14-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "84c787c604360f9058332bec8e4475f9bef729fe9b94ce42ad73dda588cc7bae"
    end
  end

  def install
    bin.install "wakezilla"
  end
end
