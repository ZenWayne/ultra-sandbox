# Generic base image: Debian + Claude Code (only curl + ca-certificates added).
#
# Build (works in bash, cmd, and PowerShell — single line, literal args).
# HOST_USER_NAME must NOT be `root`; the Dockerfile creates a non-root user
# with this name.
#
# docker build -f claude_code_base.Dockerfile --build-arg HOST_USER_UID=1000 --build-arg HOST_USER_GID=1000 --build-arg HOST_USER_NAME=devuser -t claude_code_base .

FROM debian:bookworm-slim

ARG HOST_USER_NAME
ARG HOST_USER_UID
ARG HOST_USER_GID
ARG HTTP_PROXY
ARG HTTPS_PROXY

ENV DEBIAN_FRONTEND=noninteractive
ENV HTTP_PROXY=${HTTP_PROXY}
ENV HTTPS_PROXY=${HTTPS_PROXY}
ENV http_proxy=${HTTP_PROXY}
ENV https_proxy=${HTTPS_PROXY}

# Use Aliyun mirror for Debian packages
RUN rm -f /etc/apt/sources.list.d/* && \
    echo "deb http://mirrors.aliyun.com/debian/ bookworm main contrib non-free non-free-firmware" > /etc/apt/sources.list && \
    echo "deb http://mirrors.aliyun.com/debian-security/ bookworm-security main contrib non-free non-free-firmware" >> /etc/apt/sources.list && \
    echo "deb http://mirrors.aliyun.com/debian/ bookworm-updates main contrib non-free non-free-firmware" >> /etc/apt/sources.list

# Minimum deps required by the Claude installer
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* && apt-get clean

# Create non-root user matching the host UID/GID
RUN if getent group ${HOST_USER_GID} > /dev/null 2>&1; then \
        useradd --uid ${HOST_USER_UID} --gid ${HOST_USER_GID} -m -s /bin/bash ${HOST_USER_NAME}; \
    else \
        groupadd --gid ${HOST_USER_GID} ${HOST_USER_NAME} && \
        useradd --uid ${HOST_USER_UID} --gid ${HOST_USER_GID} -m -s /bin/bash ${HOST_USER_NAME}; \
    fi

# Install sandbox binary (Linux) for command proxying to host daemon
RUN ARCH=$(dpkg --print-architecture) && \
    case "$ARCH" in \
        amd64) ASSET="sandbox-linux-x86_64" ;; \
        arm64) ASSET="sandbox-darwin-arm64" ;; \
        *) echo "unsupported arch: $ARCH" && exit 1 ;; \
    esac && \
    curl -fsSL "https://github.com/ZenWayne/ultra-sandbox/releases/latest/download/$ASSET" \
        -o /usr/local/bin/sandbox && \
    chmod 755 /usr/local/bin/sandbox

RUN mkdir -p /workspace \
    && mkdir -p /home/${HOST_USER_NAME}/.claude \
    && mkdir -p /home/${HOST_USER_NAME}/.local/bin \
    && chown -R ${HOST_USER_UID}:${HOST_USER_GID} /home/${HOST_USER_NAME} \
    && chown ${HOST_USER_UID}:${HOST_USER_GID} /workspace

USER ${HOST_USER_UID}
ENV HOME="/home/${HOST_USER_NAME}"
ENV PATH="/home/${HOST_USER_NAME}/.local/bin:${PATH}"

# Install Claude Code (native binary into ~/.local/bin)
RUN curl -fsSL https://claude.ai/install.sh | bash

WORKDIR /workspace
CMD ["claude"]
