#!/usr/bin/env bash
set -euo pipefail

cli="$1"
deb="$2"

symbols="$(nm "$cli")"
if [[ "$symbols" == *aws_lc* || "$symbols" != *ring_core* ]]; then
    echo "Expected ring and no AWS-LC in the release CLI" >&2
    exit 1
fi

libraries="$(ldd "$cli")"
printf '%s\n' "$libraries"
[[ "$libraries" == *libusb-1.0.so* ]]
if grep -Ei 'not found|lib(X11|Xext|Xi[.]|Xrandr|Xcursor|Xinerama|xcb|xkbcommon|wayland|GL|EGL|GLES|vulkan|drm|gbm|ssl|crypto)' <<< "$libraries"; then
    echo "Unexpected CLI runtime library" >&2
    exit 1
fi

depends="$(dpkg-deb -f "$deb" Depends)"
printf 'CLI DEB Depends: %s\n' "$depends"
[[ "$depends" == *ca-certificates* && "$depends" == *libusb-1.0-0* ]]
while IFS= read -r dependency; do
    case "$dependency" in
        ca-certificates|libc6|libgcc-s1|libusb-1.0-0|libudev1) ;;
        *) echo "Unexpected CLI DEB dependency: $dependency" >&2; exit 1 ;;
    esac
done < <(tr ',' '\n' <<< "$depends" | awk '{print $1}')
