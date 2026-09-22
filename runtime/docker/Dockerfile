# The Tyrion Worker image. Build it once, pin the resulting digest in the
# Worker runtime configuration, and never let Tyrion pull: provisioning is an
# explicit operator step so the image that runs is always the reviewed one.
#
#   docker build -t <registry>/tyrion-worker:<tag> runtime/docker
#   docker push   <registry>/tyrion-worker:<tag>
#   docker image inspect <registry>/tyrion-worker:<tag> \
#     --format '{{index .RepoDigests 0}} {{.Id}}'
#
# The image runs read-only as uid 65534 with no capabilities, so it must carry
# every tool a Worker needs and write nothing outside /sandbox.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      git \
      python3 \
      python3-venv \
 && rm -rf /var/lib/apt/lists/*

# Harness binaries are uploaded per Attempt and verified against their pins,
# so none are baked in here.
WORKDIR /sandbox
USER 65534:65534
