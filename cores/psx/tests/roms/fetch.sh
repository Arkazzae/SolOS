#!/bin/sh
#
# Fetches the PlayStation tests the tests run into this directory:
# ps1-tests (JaCzekanski), release build-158, checked by hash. Their logs
# (psx.log) and VRAM pictures (vram.png) come from a real console. They are
# not part of the repository.

set -e
cd "$(dirname "$0")"
if [ ! -d ps1-tests ]; then
	curl -sSL -o tests.zip https://github.com/JaCzekanski/ps1-tests/releases/download/build-158/tests.zip
	sum=5de0e82854c1a511697a7ade381c2d778ec4b2fff4a7d7ac79e8098c35eb859b
	if command -v sha256sum >/dev/null; then
		echo "$sum  tests.zip" | sha256sum -c - >/dev/null
	else
		echo "$sum  tests.zip" | shasum -a 256 -c - >/dev/null
	fi
	mkdir ps1-tests
	unzip -q tests.zip -d ps1-tests
	rm tests.zip
fi
