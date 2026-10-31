#!/usr/bin/env bash
set -euo pipefail

# the repository CI runs in, or kjanat/dprint
repository_path="${GITHUB_REPOSITORY:-kjanat/dprint}"
repository="${GITHUB_SERVER_URL:-https://github.com}/${repository_path}"
homepage=https://dprint.kjanat.dev
description='Pluggable and configurable code formatter written in Rust, forked from dprint'

usage() {
	echo "usage: $0 render TAG SHASUMS DIRECTORY | publish TAG DIRECTORY" >&2
	exit 2
}

sha() {
	local asset="kprint-$1.zip" hash
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
	cat >"${directory}/kprint.rb" <<RUBY
cask "kprint" do
  version "${tag}"

  on_macos do
    on_arm do
      sha256 "${mac_arm}"
      url "${repository}/releases/download/#{version}/kprint-aarch64-apple-darwin.zip",
        verified: "${repository#*://}/"
    end
    on_intel do
      sha256 "${mac_x64}"
      url "${repository}/releases/download/#{version}/kprint-x86_64-apple-darwin.zip",
        verified: "${repository#*://}/"
    end
  end
  on_linux do
    on_arm do
      sha256 "${linux_arm}"
      url "${repository}/releases/download/#{version}/kprint-aarch64-unknown-linux-gnu.zip",
        verified: "${repository#*://}/"
    end
    on_intel do
      sha256 "${linux_x64}"
      url "${repository}/releases/download/#{version}/kprint-x86_64-unknown-linux-gnu.zip",
        verified: "${repository#*://}/"
    end
  end

  name "kprint"
  desc "${description}"
  homepage "${homepage}"

  livecheck do
    skip "Updated on release."
  end

  binary "kprint"
  # for what calls dprint
  binary "kprint", target: "dprint"

  postflight do
    if OS.mac?
      system_command "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "#{staged_path}/kprint"]
    end
  end
  generate_completions_from_executable "kprint", "completions",
    base_name: "kprint",
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
				"64bit": { url: "\($base)/kprint-x86_64-pc-windows-msvc.zip", hash: $x64 },
				arm64: { url: "\($base)/kprint-aarch64-pc-windows-msvc.zip", hash: $arm64 }
			},
			bin: ["kprint.exe", ["kprint.exe", "dprint"]]
		}' >"${directory}/kprint.json"
}

publish_file() {
	local repo="$1" path="$2" file="$3" token="$4" endpoint previous published wanted status content sha=''
	endpoint="repos/${GITHUB_REPOSITORY_OWNER:-kjanat}/${repo}/contents/${path}"
	if previous="$(GH_TOKEN="${token}" gh api "${endpoint}?ref=master")"; then
		published="$(jq -r .content <<<"${previous}")"
		published="$(base64 -d <<<"${published}")"
		wanted="$(<"${file}")"
		if [[ "${published}" == "${wanted}" ]]; then
			echo "${repo}/${path} is up to date"
			return
		fi
		sha="$(jq -r .sha <<<"${previous}")"
	else
		status="$(jq -r .status <<<"${previous}")"
		if [[ "${status}" != 404 ]]; then
			echo "Could not read ${repo}/${path}: ${previous}" >&2
			exit 1
		fi
	fi
	content="$(base64 -w0 "${file}")"
	GH_TOKEN="${token}" gh api --method PUT "${endpoint}" \
		-f message="Update kprint to ${tag}" \
		-f branch=master \
		-f content="${content}" \
		${sha:+-f sha="${sha}"} >/dev/null
	echo "${repo}/${path} updated to ${tag}"
}

publish() {
	publish_file homebrew-tap Casks/kprint.rb "${directory}/kprint.rb" "${HOMEBREW_TAP_TOKEN:?}"
	publish_file scoop-bucket bucket/kprint.json "${directory}/kprint.json" "${SCOOP_BUCKET_TOKEN:?}"
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
