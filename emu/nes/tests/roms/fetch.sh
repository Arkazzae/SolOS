#!/bin/sh
#
# Fetches the NES test ROMs the tests run (blargg's suites, nestest and
# others, collected in christopherpow/nes-test-roms) into this directory.
# They are not part of the repository.

set -e
cd "$(dirname "$0")"
if [ -d nes-test-roms ]; then
	git -C nes-test-roms pull --ff-only
else
	git clone --depth 1 https://github.com/christopherpow/nes-test-roms.git
fi
