# Native Claude Code only needs musl; no npm lifecycle scripts run in this image.
FROM alpine@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce
COPY --chmod=0755 claude /usr/local/bin/claude
ENV DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 HOME=/tmp/claude
WORKDIR /workspace
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/claude"]
CMD ["--version"]
