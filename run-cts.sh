#!/bin/sh
set -e

fatal() {
	echo "ERROR: $@"
}

# GNU Make sources to fetch tests from
VER=4.3

command -V curl > /dev/null \
	|| fatal "curl needed to fetch tests"

cargo build

mkdir -p cts
cd cts
[ -f make-$VER.tar.gz ] || curl -LO "http://ftp.gnu.org/gnu/make/make-$VER.tar.gz"
[ -d make-$VER ] || tar -xf make-$VER.tar.gz

cd make-$VER

[ -L make ] || ln -s ../../target/debug/imake make
# [ -L make ] || ln -s /usr/bin/ckati make
# [ -L make ] || ln -s /usr/bad/gmake/bin/gmake make

[ -f Makefile ] || ./configure

set +e
make check-regression MAKETESTFLAGS=''

# kill any stray make processes
killall make

set -e

rm tests/all
