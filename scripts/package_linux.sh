#!/usr/bin/env bash
set -euo pipefail

version="$(awk -F\" '/^version =/ { print $2; exit }' Cargo.toml)"
deb_arch="$(dpkg --print-architecture)"
case "$deb_arch" in
    amd64) system=linux-x64 ;;
    *) system="linux-$deb_arch" ;;
esac
release_dir="$(realpath "${CARGO_TARGET_DIR:-target}/release")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir -p packages "$work/portable" "$work/debian"
cat > "$work/debian/control" <<'EOF'
Source: artinchip-flash

Package: artinchip-flash
Architecture: any

Package: artinchip-flash-cli
Architecture: any
EOF

archive="packages/artinchip-flash-$system.tar.gz"
cli="$release_dir/artinchip-flash"
gui="$release_dir/artinchip-flash-gui"
install -m 0755 "$cli" "$work/portable/artinchip-flash-$system"
install -m 0755 "$gui" "$work/portable/artinchip-flash-gui-$system"
install -m 0644 README.md LICENSE NOTICE THIRD-PARTY-LICENSES.txt "$work/portable/"
tar -czf "$archive" -C "$work/portable" .

elf_dependencies() {
    (cd "$work" && dpkg-shlibdeps -O "$@" | sed -n 's/^shlibs:Depends=//p')
}

build_deb() {
    local package="$1" replacement="$2" attribution="$3" description="$4"
    local root="$work/debian/$package"
    local doc="$root/usr/share/doc/$package" dependencies
    local executables=("$root/usr/bin/artinchip-flash")

    install -d "$root/DEBIAN" "$root/usr/bin" "$doc" "$root/usr/lib/udev/rules.d"
    install -m 0755 "$cli" "$root/usr/bin/artinchip-flash"
    install -m 0644 README.md LICENSE NOTICE "$doc/"
    install -m 0644 "$attribution" "$doc/THIRD-PARTY-LICENSES.txt"
    cat > "$root/usr/lib/udev/rules.d/99-artinchip-flash.rules" <<'EOF'
SUBSYSTEM=="usb", ATTR{idVendor}=="33c3", ATTR{idProduct}=="6677", TAG+="uaccess", MODE="0666"
EOF
    cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
    udevadm trigger || true
fi
EOF
    cat > "$root/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules || true
fi
EOF
    chmod 0755 "$root/DEBIAN/postinst" "$root/DEBIAN/postrm"

    if [[ "$package" == artinchip-flash ]]; then
        install -d "$root/usr/share/applications" "$root/usr/share/icons/hicolor/256x256/apps"
        install -m 0755 "$gui" "$root/usr/bin/artinchip-flash-gui"
        install -m 0644 assets/icons/artinchip-flash-256.png "$root/usr/share/icons/hicolor/256x256/apps/artinchip-flash.png"
        cat > "$root/usr/share/applications/artinchip-flash-gui.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=artinchip-flash GUI
Comment=ArtInChip firmware flashing tool
Exec=artinchip-flash-gui
Icon=artinchip-flash
Terminal=false
Categories=Utility;Development;
EOF
        executables+=("$root/usr/bin/artinchip-flash-gui")
    fi

    dependencies="$(elf_dependencies "${executables[@]}")"
    if [[ "$package" == artinchip-flash ]]; then
        dependencies+=", libx11-6, libx11-xcb1, libxi6, libxcursor1, libxrandr2, libxinerama1"
        dependencies+=", libxkbcommon0, libxkbcommon-x11-0, libgl1, libegl1"
        dependencies+=", libwayland-client0, libwayland-cursor0, libwayland-egl1"
    fi
    cat > "$root/DEBIAN/control" <<EOF
Package: $package
Version: $version
Section: utils
Priority: optional
Architecture: $deb_arch
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Maintainer: boa-w
Depends: $dependencies, ca-certificates
Conflicts: $replacement
Replaces: $replacement
Description: $description
EOF
    local deb="packages/$package-$system.deb"
    dpkg-deb --root-owner-group --build "$root" "$deb"
    test "$(dpkg-deb -f "$deb" Package)" = "$package"
    test "$(dpkg-deb -f "$deb" Version)" = "$version"
    dpkg-deb -x "$deb" "$work/verified-$package"
    cmp "$cli" "$work/verified-$package/usr/bin/artinchip-flash"
    for file in LICENSE NOTICE; do
        cmp "$file" "$work/verified-$package/usr/share/doc/$package/$file"
    done
    cmp "$attribution" "$work/verified-$package/usr/share/doc/$package/THIRD-PARTY-LICENSES.txt"
}

build_deb artinchip-flash artinchip-flash-cli THIRD-PARTY-LICENSES.txt \
    "ArtInChip firmware flashing tool with CLI and GUI"
build_deb artinchip-flash-cli artinchip-flash THIRD-PARTY-LICENSES-CLI.txt \
    "ArtInChip command-line firmware flashing tool"

for file in LICENSE NOTICE THIRD-PARTY-LICENSES.txt; do
    tar -xOf "$archive" "./$file" | cmp "$file" -
done
tar -xOf "$archive" "./artinchip-flash-$system" | cmp "$cli" -
tar -xOf "$archive" "./artinchip-flash-gui-$system" | cmp "$gui" -
