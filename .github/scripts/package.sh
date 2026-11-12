#!/bin/sh
# Pack a built mihoctl binary into dist/mihoctl-<tag>-<target>.tar.gz.
#
# `mihoctl self-update` depends on this layout: the archive holds a single directory named like the
# archive, with the binary at <dir>/mihoctl. Keep it in step with src/selfupdate.rs.
#
# usage: sh .github/scripts/package.sh <tag> <target> <path-to-binary>
set -eu

tag="$1"
target="$2"
binary="$3"
pkg="mihoctl-${tag}-${target}"

rm -rf "dist/${pkg}"
mkdir -p "dist/${pkg}"
cp "${binary}" "dist/${pkg}/mihoctl"
cp README.md "dist/${pkg}/"
# COPYFILE_DISABLE keeps bsdtar on macOS from adding AppleDouble (._*) entries.
COPYFILE_DISABLE=1 tar -C dist -czf "dist/${pkg}.tar.gz" "${pkg}"
rm -rf "dist/${pkg}"
