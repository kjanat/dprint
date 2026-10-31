#!/usr/bin/env sh
# Adapted from Deno's install script at https://github.com/denoland/deno_install/blob/HEAD/install.sh
# All rights reserved. MIT license.

set -e

if ! command -v unzip >/dev/null; then
	echo "Error: unzip is required to install kprint." 1>&2
	exit 1
fi

if ! command -v jq >/dev/null; then
	echo "Error: jq is required to resolve the kprint release repository." 1>&2
	exit 1
fi

if [ "${OS:-}" = "Windows_NT" ]; then
	case "${PROCESSOR_ARCHITECTURE:-}" in
		ARM64) target="aarch64-pc-windows-msvc" ;;
		*) target="x86_64-pc-windows-msvc" ;;
	esac
else
	case $(uname -sm) in
		"Darwin x86_64") target="x86_64-apple-darwin" ;;
		"Darwin arm64") target="aarch64-apple-darwin" ;;
		"FreeBSD amd64") target="x86_64-unknown-freebsd" ;;
		FreeBSD\ *)
			echo "Error: kprint provides FreeBSD binaries for amd64 only." 1>&2
			exit 1
			;;
		# Termux reports "Linux aarch64"/"Linux x86_64" but uses Android's bionic libc, so check uname -o.
		"Linux aarch64")
			operating_system=$(uname -o 2>/dev/null || true)
			if [ "${operating_system}" = "Android" ]; then
				target="aarch64-linux-android"
			else
				target="aarch64-unknown-linux"
			fi
			;;
		"Linux x86_64")
			operating_system=$(uname -o 2>/dev/null || true)
			if [ "${operating_system}" = "Android" ]; then
				target="x86_64-linux-android"
			else
				target="x86_64-unknown-linux"
			fi
			;;
		"Linux loongarch64") target="loongarch64-unknown-linux" ;;
		"Linux riscv64") target="riscv64gc-unknown-linux-gnu" ;; # riscv64 build only has a GNU libc variant.
		"Linux ppc64le") target="powerpc64le-unknown-linux" ;;
		*) target="x86_64-unknown-linux" ;;
	esac
fi
if [ "${target%-linux}" != "${target}" ]; then # check "-linux" suffix
	is_musl=$(ldd /bin/sh | grep 'musl' || true)
	if [ -z "${is_musl}" ]; then
		target="${target}-gnu"
	else
		target="${target}-musl"
	fi
fi

# Resolve the permanent repository ID so downloads survive repository renames.
if [ -n "${GITHUB_TOKEN:-}" ]; then
	repository_json=$(curl -fsSL --header "Authorization: Bearer ${GITHUB_TOKEN}" "https://api.github.com/repositories/1092062077")
else
	repository_json=$(curl -fsSL "https://api.github.com/repositories/1092062077")
fi
repository_url=$(printf '%s\n' "${repository_json}" | jq --exit-status --raw-output '.html_url | strings')
if [ $# -eq 0 ]; then
	release_uri="${repository_url}/releases/latest/download"
else
	release_uri="${repository_url}/releases/download/${1}"
fi

dprint_install="${DPRINT_INSTALL:-${HOME}/.dprint}"
bin_dir="${dprint_install}/bin"
if [ ! -d "${bin_dir}" ]; then
	mkdir -p "${bin_dir}"
fi
dprint_install="$(realpath "${dprint_install}")"
bin_dir="${dprint_install}/bin"

exe_suffix=""
case "${target}" in
	*-pc-windows-msvc) exe_suffix=".exe" ;;
	*) ;;
esac
exe="${bin_dir}/kprint${exe_suffix}"
alias_exe="${bin_dir}/dprint${exe_suffix}"
zip="${bin_dir}/kprint.zip"

# download; a release before kprint has dprint-<target>.zip with dprint in it
if ! curl --fail --location --progress-bar --output "${zip}" "${release_uri}/kprint-${target}.zip"; then
	curl --fail --location --progress-bar --output "${zip}" "${release_uri}/dprint-${target}.zip"
fi

# stop any running editor services
pkill -9 "kprint" || true
pkill -9 "dprint" || true

# install, with dprint for what calls that
cd "${bin_dir}"
rm -f "${alias_exe}"
unzip -o "${zip}"
if [ -e "${alias_exe}" ]; then
	mv -f "${alias_exe}" "${exe}"
fi
chmod +x "${exe}"
ln -sf "kprint${exe_suffix}" "${alias_exe}"
rm "${zip}"

echo "kprint was installed successfully to ${exe}"
if command -v kprint >/dev/null; then
	echo "Run 'kprint --help' to get started"
else
	case "${SHELL}" in
		/bin/zsh) shell_profile=".zshrc" ;;
		*) shell_profile=".bash_profile" ;;
	esac
	echo "Manually add the directory to your \$HOME/${shell_profile} (or similar)"
	echo "  export DPRINT_INSTALL=\"${dprint_install}\""
	echo "  export PATH=\"\$DPRINT_INSTALL/bin:\$PATH\""
	echo "Run '${exe} --help' to get started"
fi
