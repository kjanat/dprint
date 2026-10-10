#!/usr/bin/env bash
set -euo pipefail

repository=https://github.com/kjanat/dprint
homepage=https://dprint.kjanat.dev
description='Pluggable and configurable code formatting platform'

usage() {
	echo "usage: $0 render TAG SHASUMS DIRECTORY | publish TAG DIRECTORY" >&2
	exit 2
}

sha() {
	local asset="dprint-$1.zip" hash
	hash="$(awk -v asset="${asset}" '$2 == asset { print $1 }' "${shasums}")"
	if [[ ! "${hash}" =~ ^[0-9a-f]{64}$ ]]; then
		echo "${shasums} has no SHA-256 for ${asset}" >&2
		exit 1
	fi
	echo "${hash}"
}

render() {
	local mac_arm mac_x64 linux_arm linux_x64 windows_x64 windows_arm64
	mac_arm="$(sha aarch64-apple-darwin)"
	mac_x64="$(sha x86_64-apple-darwin)"
	linux_arm="$(sha aarch64-unknown-linux-gnu)"
	linux_x64="$(sha x86_64-unknown-linux-gnu)"
	windows_x64="$(sha x86_64-pc-windows-msvc)"
	windows_arm64="$(sha aarch64-pc-windows-msvc)"
	mkdir -p "${directory}"
	cat >"${directory}/dprint.rb" <<RUBY
cask "dprint" do
  version "${tag}"

  on_macos do
    on_arm do
      sha256 "${mac_arm}"
      url "${repository}/releases/download/#{version}/dprint-aarch64-apple-darwin.zip",
        verified: "github.com/kjanat/dprint/"
    end
    on_intel do
      sha256 "${mac_x64}"
      url "${repository}/releases/download/#{version}/dprint-x86_64-apple-darwin.zip",
        verified: "github.com/kjanat/dprint/"
    end
  end
  on_linux do
    on_arm do
      sha256 "${linux_arm}"
      url "${repository}/releases/download/#{version}/dprint-aarch64-unknown-linux-gnu.zip",
        verified: "github.com/kjanat/dprint/"
    end
    on_intel do
      sha256 "${linux_x64}"
      url "${repository}/releases/download/#{version}/dprint-x86_64-unknown-linux-gnu.zip",
        verified: "github.com/kjanat/dprint/"
    end
  end

  name "dprint"
  desc "${description}"
  homepage "${homepage}"

  livecheck do
    skip "Updated on release."
  end

  binary "dprint"

  postflight do
    if OS.mac?
      system_command "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "#{staged_path}/dprint"]
    end
  end
  generate_completions_from_executable "dprint", "completions",
    base_name: "dprint",
    shells: [:bash, :zsh, :fish]
end
RUBY
	jq -n --indent 4 \
		--arg version "${tag}" \
		--arg homepage "${homepage}" \
		--arg description "${description}" \
		--arg base "${repository}/releases/download/${tag}" \
		--arg x64 "${windows_x64}" \
		--arg arm64 "${windows_arm64}" \
		'{
			version: $version,
			description: $description,
			homepage: $homepage,
			license: "MIT",
			architecture: {
				"64bit": { url: "\($base)/dprint-x86_64-pc-windows-msvc.zip", hash: $x64 },
				arm64: { url: "\($base)/dprint-aarch64-pc-windows-msvc.zip", hash: $arm64 }
			},
			bin: "dprint.exe"
		}' >"${directory}/dprint.json"
}

publish_file() {
	local repo="$1" path="$2" file="$3" token="$4" endpoint previous sha=''
	endpoint="repos/kjanat/${repo}/contents/${path}"
	if previous="$(GH_TOKEN="${token}" gh api "${endpoint}?ref=master")"; then
		if [[ "$(jq -r .content <<<"${previous}" | base64 -d)" == "$(cat "${file}")" ]]; then
			echo "${repo}/${path} is up to date"
			return
		fi
		sha="$(jq -r .sha <<<"${previous}")"
	elif [[ "$(jq -r .status <<<"${previous}")" != 404 ]]; then
		echo "Could not read ${repo}/${path}: ${previous}" >&2
		exit 1
	fi
	GH_TOKEN="${token}" gh api --method PUT "${endpoint}" \
		-f message="Update dprint to ${tag}" \
		-f branch=master \
		-f content="$(base64 -w0 "${file}")" \
		${sha:+-f sha="${sha}"} >/dev/null
	echo "${repo}/${path} updated to ${tag}"
}

publish() {
	publish_file homebrew-tap Casks/dprint.rb "${directory}/dprint.rb" "${HOMEBREW_TAP_TOKEN:?}"
	publish_file scoop-bucket bucket/dprint.json "${directory}/dprint.json" "${SCOOP_BUCKET_TOKEN:?}"
}

case "${1:-}" in
	render)
		[[ $# == 4 ]] || usage
		tag="$2" shasums="$3" directory="$4"
		render
		;;
	publish)
		[[ $# == 3 ]] || usage
		tag="$2" directory="$3"
		publish
		;;
	*) usage ;;
esac
