#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?}" "${BUILD:?}" "${PROFILE:?}"

case "${PROFILE}" in
	debug) release='' ;;
	release) release=1 ;;
	*)
		echo "unknown profile ${PROFILE}" >&2
		exit 1
		;;
esac

# cargo-zigbuild takes the glibc version to link against as a suffix of the target.
cargo_target="${TARGET}"
cargo=cargo
if [[ "${BUILD}" == zigbuild ]]; then
	cargo_target="${TARGET}.${GLIBC:?}"
	cargo='cargo-zigbuild'
fi

setup() {
	case "${BUILD}" in
		cargo)
			rustup target add "${TARGET}"
			if [[ "${TARGET}" == *-musl ]]; then
				sudo apt-get update
				sudo apt-get install --yes musl musl-dev musl-tools
			fi
			;;
		zigbuild)
			rustup target add "${TARGET}"
			cargo install cargo-zigbuild --locked --version 0.23.0
			;;
		cross)
			cargo install cross --locked --git https://github.com/cross-rs/cross --rev 36c0d7810ddde073f603c82d896c2a6c886ff7a4
			;;
		musl-image) ;;
		*)
			echo "unknown build ${BUILD}" >&2
			exit 1
			;;
	esac
}

build() {
	case "${BUILD}" in
		cargo) cargo build -p kprint --locked --target "${TARGET}" ${release:+--release} ;;
		zigbuild) cargo zigbuild -p kprint --locked --target "${cargo_target}" ${release:+--release} ;;
		cross) cross build -p kprint --locked --target "${TARGET}" ${release:+--release} ;;
		musl-image)
			docker run --rm --volume "${PWD}:/home/rust/src" --workdir /home/rust/src "${IMAGE:?}" \
				bash -c "rustup target add ${TARGET} && cargo build -p kprint --locked --target ${TARGET} ${release:+--release}"
			sudo chown -R "$(id -u):$(id -g)" target
			;;
		*)
			echo "unknown build ${BUILD}" >&2
			exit 1
			;;
	esac
}

check_glibc() {
	local binary="target/${TARGET}/${PROFILE}/dprint" required
	required="$(objdump -T "${binary}" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sed 's/GLIBC_//' | sort -uV | tail -1)"
	echo "${binary} requires glibc ${required} (max allowed: ${GLIBC:?})"
	[[ "$(printf '%s\n%s\n' "${required}" "${GLIBC}" | sort -V | tail -1)" == "${GLIBC}" ]]
}

run_tests() {
	"${cargo}" build -p test-process-plugin --locked --target "${cargo_target}" ${release:+--release}
	"${cargo}" test --locked --target "${cargo_target}" --all-features ${release:+--release}
}

package() {
	cd "target/${TARGET}/${PROFILE}"
	if [[ "${RUNNER_OS:-}" == Windows ]]; then
		7z a -mx9 "dprint-${TARGET}.zip" dprint.exe
	else
		zip "dprint-${TARGET}.zip" dprint
	fi
}

case "${1:-}" in
	setup) setup ;;
	build) build ;;
	check-glibc) check_glibc ;;
	test) run_tests ;;
	package) package ;;
	*)
		echo "usage: $0 setup|build|check-glibc|test|package" >&2
		exit 2
		;;
esac
