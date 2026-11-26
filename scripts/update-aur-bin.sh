#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <tag-or-version> <aur-checkout-dir>" >&2
    exit 2
fi

input_version="$1"
aur_dir="$2"
pkgver="${input_version#refs/tags/}"
pkgver="${pkgver#v}"
tag="v${pkgver}"

pkgname="Wayutils-Screen-bin"
pkgrel="${AUR_PKGREL:-1}"
project_url="https://github.com/shlok2721-web/Wayutils-Screen"
asset="Wayutils-Screen-archlinux-x86_64.zip"
asset_url="${project_url}/releases/download/${tag}/${asset}"
source_name="${pkgname}-${pkgver}.zip"
pkgdesc="A scrolling screenshot tool for Wayland"

if [[ ! -d "$aur_dir/.git" ]]; then
    echo "AUR checkout not found: $aur_dir" >&2
    exit 2
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

curl -fsSL -o "${tmpdir}/${asset}" "$asset_url"
sha256="$(sha256sum "${tmpdir}/${asset}" | awk '{print $1}')"

cat >"${aur_dir}/PKGBUILD" <<EOF
# Maintainer: Snemc-s <snemc@snemc.cn>
pkgname=${pkgname}
pkgver=${pkgver}
pkgrel=${pkgrel}
pkgdesc="${pkgdesc}"
arch=('x86_64')
url="${project_url}"
license=('MIT')
depends=('gcc-libs' 'glibc' 'libxkbcommon' 'opencv' 'slurp')
optdepends=(
    'grim: capture frames on wlroots compositors'
    'gnome-shell: GNOME Shell scrolling-capture backend'
    'wl-clipboard: copy screenshots to the Wayland clipboard'
    'xclip: X11 clipboard fallback'
    'ffmpeg: MP4 screen recording'
)
provides=("Wayutils-Screen=\${pkgver}")
conflicts=('Wayutils-Screen' 'Wayutils-Screen-git')
source=("${source_name}::\${url}/releases/download/v\${pkgver}/${asset}")
sha256sums=('${sha256}')

package() {
    install -Dm755 Wayutils-Screen "\${pkgdir}/usr/bin/Wayutils-Screen"
    install -Dm644 Wayutils-Screen.desktop "\${pkgdir}/usr/share/applications/Wayutils-Screen.desktop"
    install -Dm644 wayutils-screen.svg "\${pkgdir}/usr/share/icons/hicolor/scalable/apps/wayutils-screen.svg"
    install -Dm644 LICENSE "\${pkgdir}/usr/share/licenses/\${pkgname}/LICENSE"
    install -Dm644 gnome-extension/Wayutils-Screen-scroll-helper@snemc.org/metadata.json \
        "\${pkgdir}/usr/share/gnome-shell/extensions/Wayutils-Screen-scroll-helper@snemc.org/metadata.json"
    install -Dm644 gnome-extension/Wayutils-Screen-scroll-helper@snemc.org/extension.js \
        "\${pkgdir}/usr/share/gnome-shell/extensions/Wayutils-Screen-scroll-helper@snemc.org/extension.js"
}
EOF

cat >"${aur_dir}/.SRCINFO" <<EOF
pkgbase = ${pkgname}
	pkgdesc = ${pkgdesc}
	pkgver = ${pkgver}
	pkgrel = ${pkgrel}
	url = ${project_url}
	arch = x86_64
	license = MIT
	depends = gcc-libs
	depends = glibc
	depends = libxkbcommon
	depends = opencv
	depends = slurp
	optdepends = grim: capture frames on wlroots compositors
	optdepends = gnome-shell: GNOME Shell scrolling-capture backend
	optdepends = wl-clipboard: copy screenshots to the Wayland clipboard
	optdepends = xclip: X11 clipboard fallback
	optdepends = ffmpeg: MP4 screen recording
	provides = Wayutils-Screen=${pkgver}
	conflicts = Wayutils-Screen
	conflicts = Wayutils-Screen-git
	source = ${source_name}::${asset_url}
	sha256sums = ${sha256}

pkgname = ${pkgname}
EOF

echo "Updated ${pkgname} to ${pkgver}"
