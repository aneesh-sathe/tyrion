# The Tyrion Worker image: the disposable environment every Attempt, every
# verification run, and every comparison executes in.
#
# Beyond a base toolchain it carries the runtime dependencies Tyrion's adapters
# import and the harness binaries themselves, under /opt/tyrion/harness. Baking
# the harnesses in means every Worker shares one read-only copy instead of
# streaming its own into its memory-charged tmpfs, which roughly halves a
# Worker's footprint. The image ID pins all of it: Tyrion refuses to start if
# the image is absent and refuses to run if a container launched anything else.
#
# `tyrion init` builds it. The build context holds this file, the adapter
# helper at adapters/native_skill.py, and the checksum-verified Linux harness
# binaries in harness/ (codex, codex-code-mode-host, claude).
FROM debian:bookworm-slim

# `python` as well as `python3`: host models write acceptance checks the way
# developers type them, and a verifier that cannot start blocks its Assignment.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      git \
      python3 \
      python3-pip \
      python-is-python3 \
 && rm -rf /var/lib/apt/lists/*

# This image exists only to be a throwaway sandbox, so there is no system
# Python installation to protect and no reason to add a virtualenv layer.
RUN pip install --no-cache-dir --break-system-packages claude-agent-sdk

# Tyrion's own adapter helper. Every adapter imports it, so it belongs to the
# environment rather than being transferred per Attempt with a second hash to
# keep in step. Reached through PYTHONPATH, which the sandbox sets.
COPY adapters/native_skill.py /opt/tyrion/native_skill.py

# Read-only and executable by the unprivileged Worker user. Codex finds its
# code-mode host beside itself.
COPY --chmod=0555 harness/ /opt/tyrion/harness/

# The only writable path is the /sandbox tmpfs the sandbox mounts at runtime.
WORKDIR /sandbox
USER 65534:65534
