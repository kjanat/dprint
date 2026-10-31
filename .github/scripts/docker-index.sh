#!/usr/bin/env bash
# Pushes the release zips in a directory as one OCI index that's both an image
# and a store of the binaries:
#
#   docker run ghcr.io/kjanat/kprint:<tag> fmt
#   oras pull --platform linux/amd64 ghcr.io/kjanat/kprint:<tag>  # ./usr/local/bin/dprint
#   oras pull --platform darwin/arm64 ghcr.io/kjanat/kprint:<tag> # ./dprint-aarch64-apple-darwin.zip
#
# linux/amd64, linux/arm64 and linux/ppc64le are distroless images with the
# static musl build, whose layer of it ORAS unpacks (its base's layers have no
# file name, so ORAS skips them). Every other zip is an artifact of its
# platform, which has no image.
#
# Usage: docker-index.sh <image> <zips dir> <tag>... (logged in with oras)
# With ORAS_OCI_LAYOUT=1, <image> is an OCI image layout directory to try it.
set -euo pipefail
shopt -s inherit_errexit

push_flags=()
[[ "${ORAS_OCI_LAYOUT:-}" == 1 ]] && push_flags=(--oci-layout)

image="$1"
zips="$2"
shift 2
base=gcr.io/distroless/static-debian13:nonroot
source_url="${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY:-kjanat/dprint}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

oci_manifest=application/vnd.oci.image.manifest.v1+json
oci_index=application/vnd.oci.image.index.v1+json
oci_layer=application/vnd.oci.image.layer.v1.tar+gzip
empty_digest=sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a

digest() {
  local sum
  sum="$(sha256sum < "$1")"
  echo "sha256:${sum%% *}"
}
size() { stat -c %s "$1"; }

# pushes a manifest by its digest, printing its descriptor
push_manifest() {
  local file="$1" platform="$2" artifact_type="${3:-}"
  local file_digest file_size
  file_digest="$(digest "${file}")"
  file_size="$(size "${file}")"
  oras manifest push "${push_flags[@]}" --media-type "${oci_manifest}" "${image}@${file_digest}" "${file}" > /dev/null
  jq -nc --arg digest "${file_digest}" --argjson size "${file_size}" --argjson platform "${platform}" \
    --arg artifact_type "${artifact_type}" --arg media_type "${oci_manifest}" \
    '{mediaType: $media_type, digest: $digest, size: $size, platform: $platform}
      + if $artifact_type == "" then {} else {artifactType: $artifact_type} end'
}

image_manifest() {
  local arch="$1" zip="$2"
  local dir="${work}/${arch}"
  mkdir -p "${dir}/root/usr/local/bin"
  unzip -q -o "${zip}" dprint -d "${dir}/root/usr/local/bin"
  chmod 0755 "${dir}/root/usr/local/bin/dprint"

  # the base's config and layers, as OCI
  oras manifest fetch --platform "linux/${arch}" "${base}" > "${dir}/base.json"
  local config_digest
  config_digest="$(jq -r .config.digest "${dir}/base.json")"
  oras blob fetch --output "${dir}/base-config.json" "${base%:*}@${config_digest}"
  local layers='[]' layer_digests layer_digest layer_size
  layer_digests="$(jq -r '.layers[].digest' "${dir}/base.json")"
  for layer_digest in ${layer_digests}; do
    oras blob fetch --output "${dir}/layer" "${base%:*}@${layer_digest}"
    oras blob push "${push_flags[@]}" "${image}" "${dir}/layer" > /dev/null
    layer_size="$(size "${dir}/layer")"
    layers="$(jq -c --arg digest "${layer_digest}" --argjson size "${layer_size}" --arg media_type "${oci_layer}" \
      '. + [{mediaType: $media_type, digest: $digest, size: $size}]' <<< "${layers}")"
  done

  # the binary's layer, which ORAS unpacks into ./usr. Its digest annotation
  # is of what a tar reader reads, which is no further than the end of the
  # archive, so it's written without padding after it.
  tar --create --file "${dir}/dprint.tar" --directory "${dir}/root" --sort=name --mtime=@0 \
    --owner=0 --group=0 --numeric-owner --mode=0755 \
    --blocking-factor=1 usr
  gzip -9 -n -c "${dir}/dprint.tar" > "${dir}/dprint.tar.gz"
  local tar_digest gz_digest gz_size
  tar_digest="$(digest "${dir}/dprint.tar")"
  gz_digest="$(digest "${dir}/dprint.tar.gz")"
  gz_size="$(size "${dir}/dprint.tar.gz")"
  oras blob push "${push_flags[@]}" "${image}" "${dir}/dprint.tar.gz" > /dev/null
  layers="$(jq -c --arg digest "${gz_digest}" --argjson size "${gz_size}" \
    --arg tar_digest "${tar_digest}" --arg media_type "${oci_layer}" \
    '. + [{mediaType: $media_type, digest: $digest, size: $size, annotations: {
      "org.opencontainers.image.title": "usr",
      "io.deis.oras.content.unpack": "true",
      "io.deis.oras.content.digest": $tar_digest
    }}]' <<< "${layers}")"

  jq -c --arg diff_id "${tar_digest}" '
    .rootfs.diff_ids += [$diff_id]
    | .config.Entrypoint = ["/usr/local/bin/dprint"]
    | .config.Cmd = null
    | .config.WorkingDir = "/work"
    | .history += [{created: "1970-01-01T00:00:00Z", created_by: "COPY dprint /usr/local/bin/dprint"}]
  ' "${dir}/base-config.json" > "${dir}/config.json"
  oras blob push "${push_flags[@]}" "${image}" "${dir}/config.json" > /dev/null

  local new_config_digest new_config_size platform
  new_config_digest="$(digest "${dir}/config.json")"
  new_config_size="$(size "${dir}/config.json")"
  jq -nc --arg config_digest "${new_config_digest}" --argjson config_size "${new_config_size}" \
    --argjson layers "${layers}" --arg source "${source_url}" --arg media_type "${oci_manifest}" '{
      schemaVersion: 2,
      mediaType: $media_type,
      config: {mediaType: "application/vnd.oci.image.config.v1+json", digest: $config_digest, size: $config_size},
      layers: $layers,
      annotations: {"org.opencontainers.image.source": $source}
    }' > "${dir}/manifest.json"
  platform="$(jq -nc --arg arch "${arch}" '{os: "linux", architecture: $arch}')"
  push_manifest "${dir}/manifest.json" "${platform}"
}

artifact_manifest() {
  local platform="$1" zip="$2" name dir zip_digest zip_size
  name="$(basename "${zip}")"
  dir="${work}/${name}"
  mkdir -p "${dir}"
  zip_digest="$(digest "${zip}")"
  zip_size="$(size "${zip}")"
  oras blob push "${push_flags[@]}" "${image}" "${zip}" > /dev/null
  jq -nc --arg digest "${zip_digest}" --argjson size "${zip_size}" --arg name "${name}" \
    --arg source "${source_url}" --arg media_type "${oci_manifest}" --arg empty "${empty_digest}" '{
      schemaVersion: 2,
      mediaType: $media_type,
      artifactType: "application/vnd.dprint.release.zip",
      config: {mediaType: "application/vnd.oci.empty.v1+json", digest: $empty, size: 2},
      layers: [{mediaType: "application/zip", digest: $digest, size: $size, annotations: {"org.opencontainers.image.title": $name}}],
      annotations: {"org.opencontainers.image.source": $source}
    }' > "${dir}/manifest.json"
  push_manifest "${dir}/manifest.json" "${platform}" application/vnd.dprint.release.zip
}

# the OCI platform of a target, which a gnu build only gets without a musl one
platform_of() {
  local target="$1" arch os
  case "${target}" in
    x86_64-*) arch=amd64 ;;
    aarch64-*) arch=arm64 ;;
    riscv64gc-*) arch=riscv64 ;;
    loongarch64-*) arch=loong64 ;;
    powerpc64le-*) arch=ppc64le ;;
    *) echo "Unknown target ${target}" >&2; return 1 ;;
  esac
  case "${target}" in
    *-apple-darwin) os=darwin ;;
    *-pc-windows-msvc) os=windows ;;
    *-unknown-freebsd) os=freebsd ;;
    *-linux-android) os=android ;;
    *-unknown-linux-*) os=linux ;;
    *) echo "Unknown target ${target}" >&2; return 1 ;;
  esac
  jq -nc --arg os "${os}" --arg arch "${arch}" '{os: $os, architecture: $arch}'
}

printf '{}' > "${work}/empty.json"
empty="$(digest "${work}/empty.json")"
[[ "${empty}" == "${empty_digest}" ]]
oras blob push "${push_flags[@]}" "${image}" "${work}/empty.json" > /dev/null

manifests='[]'
add() { manifests="$(jq -c --argjson descriptor "$1" '. + [$descriptor]' <<< "${manifests}")"; }
shopt -s nullglob
for zip in "${zips}"/dprint-*.zip; do
  target="$(basename "${zip}" .zip)"
  target="${target#dprint-}"
  case "${target}" in
    x86_64-unknown-linux-musl) descriptor="$(image_manifest amd64 "${zip}")" ;;
    aarch64-unknown-linux-musl) descriptor="$(image_manifest arm64 "${zip}")" ;;
    powerpc64le-unknown-linux-musl) descriptor="$(image_manifest ppc64le "${zip}")" ;;
    # the musl images and builds are the Linux ones
    x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) continue ;;
    *)
      if [[ "${target}" == *-unknown-linux-gnu && -e "${zips}/dprint-${target%-gnu}-musl.zip" ]]; then
        continue
      fi
      platform="$(platform_of "${target}")"
      descriptor="$(artifact_manifest "${platform}" "${zip}")"
      ;;
  esac
  add "${descriptor}"
done
jq -e 'any(.platform.os == "linux" and .artifactType == null)' <<< "${manifests}" > /dev/null \
  || { echo "No Linux musl build in ${zips}." >&2; exit 1; }

jq -n --argjson manifests "${manifests}" --arg source "${source_url}" --arg media_type "${oci_index}" '{
  schemaVersion: 2,
  mediaType: $media_type,
  manifests: $manifests,
  annotations: {"org.opencontainers.image.source": $source}
}' > "${work}/index.json"
cat "${work}/index.json"
oras manifest push "${push_flags[@]}" --media-type "${oci_index}" "${image}:$1" "${work}/index.json"
for tag in "${@:2}"; do
  oras tag "${push_flags[@]}" "${image}:$1" "${tag}"
done
