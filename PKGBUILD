# Maintainer: Ourongxing

pkgname=omash
pkgver=0.1.0
pkgrel=1
pkgdesc='Terminal dashboard for Mihomo on Omarchy'
arch=('x86_64')
url='https://github.com/ourongxing/omash'
license=('GPL-3.0-only')
depends=('mihomo' 'clash-geoip')
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
b2sums=('SKIP')

prepare() {
  cd "$pkgname-$pkgver"
  cargo fetch --locked --target "$CARCH-unknown-linux-gnu"
}

build() {
  cd "$pkgname-$pkgver"
  cargo build --frozen --release
}

check() {
  cd "$pkgname-$pkgver"
  cargo test --frozen
}

package() {
  cd "$pkgname-$pkgver"

  install -Dm755 target/release/omash "$pkgdir/usr/bin/omash"
  install -Dm644 systemd/omash-supervisor.service \
    "$pkgdir/usr/lib/systemd/user/omash-supervisor.service"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 themes/default.toml \
    "$pkgdir/usr/share/$pkgname/themes/default.toml"
  install -Dm644 integrations/omarchy/ourongxing.omash/manifest.json \
    "$pkgdir/usr/share/$pkgname/omarchy/ourongxing.omash/manifest.json"
  install -Dm644 integrations/omarchy/ourongxing.omash/Panel.qml \
    "$pkgdir/usr/share/$pkgname/omarchy/ourongxing.omash/Panel.qml"
  install -Dm644 integrations/omarchy/ourongxing.omash/README.md \
    "$pkgdir/usr/share/$pkgname/omarchy/ourongxing.omash/README.md"
}
