# Maintainer: Jérôme Poulin <jeromepoulin@gmail.com>
# Builds the checked-out tree: run makepkg -si from the repository root.

pkgname=cc-link
pkgver=0.1.0
pkgrel=1
pkgdesc='Bridge Claude Code cross-session messaging between two machines over SSH'
arch=('x86_64' 'aarch64')
url='https://github.com/ticpu/cc-link'
license=('GPL-3.0-only')
depends=('gcc-libs' 'openssh')
makedepends=('cargo')
options=('!lto')

pkgver() {
	# The version lives in Cargo.toml and nowhere else.
	sed -n 's/^version = "\(.*\)"/\1/p' "$startdir/Cargo.toml" | head -1
}

build() {
	cd "$startdir"
	cargo build --release --bin cc-link
}

check() {
	cd "$startdir"
	cargo test --release
}

package() {
	install -Dm755 "$startdir/target/release/cc-link" "$pkgdir/usr/bin/cc-link"
	install -Dm644 "$startdir/LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
	install -Dm644 "$startdir/README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
}
