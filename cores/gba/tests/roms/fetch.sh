#!/bin/sh
#
# Fetches the Game Boy Advance test ROMs the tests run into this directory,
# each collection at a fixed commit: jsmolka's (CPU, memory, BIOS, saves,
# PPU), alyosha-tas's (timing, DMA, interrupts, prefetch), NanoBoyAdvance's
# hardware tests, FuzzARM and ARMWrestler, and the demos of Tonc (the GBA
# programming tutorial), checked by hash since their file has no version.
# They are not part of the repository.

set -e
cd "$(dirname "$0")"
fetch() {
	dir=$1 repo=$2 commit=$3
	if [ ! -d "$dir" ]; then
		mkdir "$dir"
		curl -sSL "https://codeload.github.com/$repo/tar.gz/$commit" | tar -xz --strip-components 1 -C "$dir"
	fi
}
fetch gba-tests jsmolka/gba-tests a7113b67e63f83a9b321696ddd7042ccfad6c881
fetch alyosha alyosha-tas/gba-tests 66f5f1d60cd7030b600606886c76f5c821ecaf9d
fetch hw-test nba-emu/hw-test 3932a0e54dbb5d94b84798d2d5e462e080b95599
fetch fuzzarm DenSinH/FuzzARM a675329cd57da48e3e406216ba2d79dd7e09ee20
fetch armwrestler destoer/armwrestler-gba-fixed 802e55ad61b421f4bbbc6f74f96aa4e5df4d630f
if [ ! -d tonc ]; then
	curl -sSL -o tonc.zip https://www.coranac.com/files/tonc-bin.zip
	sum=f172fc9308eb030ae8d3479872e9f3521b7f0cae3a64b0cd5f526b7b4b1650ba
	if command -v sha256sum >/dev/null; then
		echo "$sum  tonc.zip" | sha256sum -c - >/dev/null
	else
		echo "$sum  tonc.zip" | shasum -a 256 -c - >/dev/null
	fi
	mkdir tonc
	unzip -q tonc.zip -d tonc
	rm tonc.zip
fi
