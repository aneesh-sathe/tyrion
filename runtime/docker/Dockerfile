# The Tyrion Worker image: the disposable environment every Attempt, every
# verification run, and every comparison executes in.
#
# It carries exactly two things beyond a base toolchain: the runtime
# dependencies Tyrion's adapters import, and nothing else. Harness binaries
# (codex, claude, pi) are streamed in per Attempt and checked against their
# own hashes, so none are baked in here.
#
# Build from the repository root so the adapter sources are in context:
#
#   docker build -f runtime/docker/Dockerfile -t <registry>/tyrion-worker:<tag> .
#   docker image inspect <registry>/tyrion-worker:<tag> \
#     --format '{{index .RepoDigests 0}}{{"\n"}}{{.Id}}'
#
# Put the digest in `worker_image` and the id in `worker_image_id`. That digest
# is the pin of record: Tyrion refuses to start if the image is absent and
# refuses to run if a container launched anything else, so every byte below is
# covered by a hash it verifies.
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      git \
      python3 \
      python3-pip \
 && rm -rf /var/lib/apt/lists/*

# This image exists only to be a throwaway sandbox, so there is no system
# Python installation to protect and no reason to add a virtualenv layer.
RUN pip install --no-cache-dir --break-system-packages claude-agent-sdk

# Tyrion's own adapter helper. Every adapter imports it, so it belongs to the
# environment rather than being transferred per Attempt with a second hash to
# keep in step. Reached through PYTHONPATH, which the sandbox sets.
COPY adapters/native_skill.py /opt/tyrion/native_skill.py

# The only writable path is the /sandbox tmpfs the sandbox mounts at runtime.
WORKDIR /sandbox
USER 65534:65534
