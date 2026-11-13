#!/bin/sh
#
# Fetches the Game Boy test ROMs the tests run (blargg's, Mooneye's,
# dmg-acid2 and others, collected in c-sp/game-boy-test-roms) into this
# directory. They are not part of the repository.

set -e
cd "$(dirname "$0")"
version=v7.0
if [ ! -d game-boy-test-roms ]; then
	curl -sSL -o roms.zip "https://github.com/c-sp/game-boy-test-roms/releases/download/$version/game-boy-test-roms-$version.zip"
	mkdir game-boy-test-roms
	unzip -q roms.zip -d game-boy-test-roms
	rm roms.zip
fi
