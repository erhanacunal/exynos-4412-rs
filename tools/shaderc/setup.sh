#!/bin/sh
# Builds what tools/shaderc runs GLSL through, once: libdrm 2.4.131 and Mesa 25.3.3 with its
# lima driver and lima's drm-shim, which stands in for a Mali-400 MP4 on this PC. Nothing here
# is installed on the system: all goes under $SHADERC_HOME (default ~/.cache/exynos-shaderc).
#
# Needs gcc, meson, ninja, pkg-config, bison, flex and a Python 3 with mako. Buildroot's host
# tools have the last three: BUILDROOT_HOST (default ~/buildroot/output/host) is put first on
# the PATH when it exists. The tarballs come from BUILDROOT_DL (default ~/buildroot/dl) when
# there, else from upstream; both are checked against these SHA-256 sums.
set -eu

SHADERC_HOME=${SHADERC_HOME:-$HOME/.cache/exynos-shaderc}
BUILDROOT_HOST=${BUILDROOT_HOST:-$HOME/buildroot/output/host}
BUILDROOT_DL=${BUILDROOT_DL:-$HOME/buildroot/dl}

MESA=mesa-25.3.3
MESA_SHA256=05328b3891c000e6a110a3e7321d8bfbb21631d132bf86bd3d4a8f45c535ef6b
MESA_URL=https://archive.mesa3d.org/$MESA.tar.xz
LIBDRM=libdrm-2.4.131
LIBDRM_SHA256=45ba9983b51c896406a3d654de81d313b953b76e6391e2797073d543c5f617d5
LIBDRM_URL=https://dri.freedesktop.org/libdrm/$LIBDRM.tar.xz

PREFIX=$SHADERC_HOME/prefix
SRC=$SHADERC_HOME/src
mkdir -p "$PREFIX" "$SRC" "$SHADERC_HOME/bin"

# Only bison, flex and python3 from buildroot: its meson and ninja may not suit this PC.
if [ -d "$BUILDROOT_HOST/bin" ]; then
    for tool in bison flex python3; do
        if [ -x "$BUILDROOT_HOST/bin/$tool" ]; then
            ln -sf "$BUILDROOT_HOST/bin/$tool" "$SHADERC_HOME/bin/$tool"
        fi
    done
fi
export PATH="$SHADERC_HOME/bin:$PATH"
python3 -c 'import mako' 2>/dev/null || { echo "setup: python3 has no mako (pip install mako, or set BUILDROOT_HOST)" >&2; exit 1; }

# fetch NAME SHA256 URL DIR: the tarball into $SRC, checked.
fetch() {
    tarball=$SRC/$1.tar.xz
    if [ ! -f "$tarball" ]; then
        if [ -f "$BUILDROOT_DL/$4/$1.tar.xz" ]; then
            cp "$BUILDROOT_DL/$4/$1.tar.xz" "$tarball"
        else
            curl -fL -o "$tarball" "$3"
        fi
    fi
    echo "$2  $tarball" | sha256sum -c - >/dev/null || { echo "setup: $tarball: wrong SHA-256" >&2; exit 1; }
}

fetch "$LIBDRM" "$LIBDRM_SHA256" "$LIBDRM_URL" libdrm
fetch "$MESA" "$MESA_SHA256" "$MESA_URL" mesa3d

if [ ! -f "$PREFIX/.libdrm-done" ]; then
    rm -rf "${SRC:?}/$LIBDRM"
    tar -C "$SRC" -xf "$SRC/$LIBDRM.tar.xz"
    (cd "$SRC/$LIBDRM" &&
        meson setup build --prefix="$PREFIX" --libdir=lib -Dintel=disabled -Dradeon=disabled -Damdgpu=disabled \
            -Dnouveau=disabled -Dvmwgfx=disabled -Dtests=false -Dman-pages=disabled -Dvalgrind=disabled \
            -Dcairo-tests=disabled &&
        ninja -C build install)
    touch "$PREFIX/.libdrm-done"
fi

if [ ! -d "$SRC/$MESA" ]; then
    tar -C "$SRC" -xf "$SRC/$MESA.tar.xz"
fi
# Two changes, each safe to make again. The shim plays a Mali-450 with six PPs by default; the
# board's is a Mali-400 MP4. And the dump prints floats as %a, exactly, not %f's six decimals,
# so that the shaders' constants come out whole.
shim=$SRC/$MESA/src/gallium/drivers/lima/drm-shim/lima_noop.c
sed -i 's/gp->value = DRM_LIMA_PARAM_GPU_ID_MALI450;/gp->value = DRM_LIMA_PARAM_GPU_ID_MALI400;/; s/gp->value = 6;/gp->value = 4;/' "$shim"
grep -q DRM_LIMA_PARAM_GPU_ID_MALI400 "$shim" || { echo "setup: could not make the shim a Mali-400" >&2; exit 1; }
util=$SRC/$MESA/src/gallium/drivers/lima/lima_util.c
sed -i 's/fprintf(fp, "%f, ", ((float \*)data)\[i\]);/fprintf(fp, "%a, ", ((float *)data)[i]);/' "$util"
grep -q '"%a, "' "$util" || { echo "setup: could not make the dump print exact floats" >&2; exit 1; }
if [ ! -f "$SRC/$MESA/build/build.ninja" ]; then
    (cd "$SRC/$MESA" &&
        PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" meson setup build --prefix="$PREFIX" --libdir=lib \
            -Dgallium-drivers=lima -Dvulkan-drivers= -Dtools=drm-shim -Dllvm=disabled -Dplatforms= \
            -Dglx=disabled -Degl=enabled -Dgbm=enabled -Dgles2=enabled -Dgles1=disabled \
            -Dvalgrind=disabled -Dlibunwind=disabled -Dzstd=disabled -Dexpat=disabled \
            -Dbuildtype=debugoptimized)
fi
ninja -C "$SRC/$MESA/build" install

# The harness, against the system's libglvnd, which shaderc.py points at Mesa's EGL vendor.
cc -O1 -Wall -o "$SHADERC_HOME/harness" "$(dirname "$0")/harness.c" -lEGL -lGLESv2
echo "setup: done, in $SHADERC_HOME"
