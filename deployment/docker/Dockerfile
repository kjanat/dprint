# The static musl builds, which docker.yml puts in bin/<arch>.
FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETARCH
COPY --chmod=0755 bin/${TARGETARCH}/dprint /usr/local/bin/dprint
WORKDIR /work
ENTRYPOINT ["dprint"]
