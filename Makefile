NAME := cc-link
BINARY := cc-link
CARGO_VERSION := $(shell grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)
VERSION := v$(CARGO_VERSION)
GIT_DIRTY := $(shell git diff-index --quiet HEAD -- . 2>/dev/null || echo dirty)
GIT_TAG := $(shell git describe --exact-match --tags 2>/dev/null | grep -E '^v')
GIT_VERSION := $(shell git log --oneline . | wc -l)-$(shell git rev-parse --short HEAD)
BASE_VERSION := $(if $(GIT_DIRTY),$(VERSION)+$(GIT_VERSION),$(if $(GIT_TAG),$(VERSION),$(VERSION)+$(GIT_VERSION)))
DEB_VERSION := $(patsubst v%,%,$(BASE_VERSION))
# Same version, in the one shape pacman accepts: no hyphens, that separator is pkgrel's.
ARCH_VERSION := $(subst -,.,$(DEB_VERSION))
DEB_AMD64 := $(NAME)_$(DEB_VERSION)_amd64.deb
DEB_ARM64 := $(NAME)_$(DEB_VERSION)_arm64.deb
BINARIES := dist/$(BINARY).amd64 dist/$(BINARY).arm64
FLOORS := dist/glibc-floor.amd64 dist/glibc-floor.arm64
PKG := package.tmp

.PHONY: all clean binaries deb deb-amd64 deb-arm64 archpkg archpkg-install

all: binaries

binaries: $(BINARIES)

deb: deb-amd64 deb-arm64

deb-amd64: $(DEB_AMD64)

deb-arm64: $(DEB_ARM64)

define build-deb
	rm -rf "$(PKG)"
	install -D -m 755 -T "$<" "$(PKG)/usr/bin/$(BINARY)"
	install -D -m 644 -T DEBIAN/control "$(PKG)/DEBIAN/control"
	sed -i -e "s/^Version:.*/Version: $(DEB_VERSION)/" \
		-e "s/^Architecture:.*/Architecture: $(1)/" \
		-e "s/^Depends:.*/Depends: libc6 (>= $$(cat dist/glibc-floor.$(1))), libgcc-s1, openssh-client/" \
		"$(PKG)/DEBIAN/control"
	@if grep -rq "$$PWD" "$(PKG)"; then echo "ERROR: package contains build path ($$PWD)" >&2; exit 1; fi
	dpkg-deb --build --root-owner-group "$(PKG)" "$@"
	rm -rf "$(PKG)"
endef

$(DEB_AMD64): dist/$(BINARY).amd64 dist/glibc-floor.amd64 DEBIAN/control
	$(call build-deb,amd64)

$(DEB_ARM64): dist/$(BINARY).arm64 dist/glibc-floor.arm64 DEBIAN/control
	$(call build-deb,arm64)

# One container build cross-compiles both, so they share a single rule
$(BINARIES) $(FLOORS) &: build.sh Containerfile Cargo.toml $(wildcard Cargo.lock) $(wildcard src/*)
	./build.sh

PKGBUILD: PKGBUILD.in Cargo.toml
	sed -e 's/@PKGVER@/$(ARCH_VERSION)/' PKGBUILD.in > $@

archpkg: PKGBUILD
	makepkg -f

# -f because the version only moves with a commit: without it makepkg would find the package of a
# dirty tree already built and install that instead of what is in front of it.
archpkg-install: PKGBUILD
	makepkg -sif

clean:
	rm -rf "$(PKG)" dist *.deb PKGBUILD *.pkg.tar.zst
