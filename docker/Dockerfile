# Seek Hub: one static musl binary in an empty image (no shell, no package manager).
# Build the binaries first with scripts/build-hub-image.ps1; it fills docker/dist/{amd64,arm64}/.
# Then, in this folder:  docker build --build-arg TARGETARCH=arm64 -t ghcr.io/calinasandu/seek-hub:0.1.0 .
FROM scratch
ARG TARGETARCH
COPY --chmod=0555 dist/${TARGETARCH}/seek-hub /seek-hub
USER 10001:10001
# Only what the packaging fixes. Name, quotas and the rest come from `init` (hub.json); compose passes
# SEEK_HUB_PUBLIC / SEEK_HUB_LAN, which win over hub.json when they are not empty.
# PATH=/ lets `docker exec seek-hub seek-hub <cmd>` find /seek-hub in an image without a shell.
ENV PATH=/ \
    SEEK_HUB_LISTEN="127.0.0.1:47321" \
    SEEK_HUB_DATA="/data" \
    SEEK_HUB_KEYS="/keys"
EXPOSE 47321
ENTRYPOINT ["/seek-hub"]
CMD ["serve"]
