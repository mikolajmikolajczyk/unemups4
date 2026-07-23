# BASE STAGE: Minimal install for what is required for the SDK to run
FROM ubuntu:20.04 AS base

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      build-essential \
      clang \
      libicu66 \
      libssl1.1 \
      lld \
      llvm \
      git \
      openssh-client \
      ca-certificates \
      sudo \
      nano \
      vim \
      unzip && \
    rm -rf /var/lib/apt/lists/*

# SETUP STAGE: Minimal install for downloading/setting up the SDK
FROM ubuntu:20.04 as setup

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      unzip \
      tar && \
    rm -rf /var/lib/apt/lists/*

ENV OO_PS4_TOOLCHAIN=/lib/OpenOrbisSDK
ARG OO_TOOLCHAIN_VERSION=0.5.3

# Download and extract sequence (ZIP -> TAR -> Content)
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

WORKDIR ${OO_PS4_TOOLCHAIN}

RUN echo "Downloading version: v$OO_TOOLCHAIN_VERSION" && \
    curl -L "https://github.com/OpenOrbis/OpenOrbis-PS4-Toolchain/releases/download/v$OO_TOOLCHAIN_VERSION/toolchain-llvm-18.2.zip" -o toolchain.zip && \
    unzip -q toolchain.zip && \
    tar -xzf toolchain-llvm-18.tar.gz --strip-components=2 && \
    rm toolchain.zip toolchain-llvm-18.tar.gz

# RUNTIME STAGE: The final stage
FROM base as runtime

ENV OO_PS4_TOOLCHAIN=/lib/OpenOrbisSDK
ENV PATH=$OO_PS4_TOOLCHAIN:$OO_PS4_TOOLCHAIN/bin/linux:$PATH

ARG OO_TOOLCHAIN_VERSION=0.5.3
ENV OO_TOOLCHAIN_VERSION=$OO_TOOLCHAIN_VERSION

COPY --from=setup ${OO_PS4_TOOLCHAIN} ${OO_PS4_TOOLCHAIN}

# Create non-root user 'orbis' with home directory
RUN groupadd -g 1000 orbis && \
    useradd -m -s /bin/bash -u 1000 -g orbis orbis && \
    echo "orbis ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/orbis && \
    chmod 0440 /etc/sudoers.d/orbis

# Fix ownership
RUN chown -R orbis:orbis ${OO_PS4_TOOLCHAIN}

USER orbis
WORKDIR /home/orbis