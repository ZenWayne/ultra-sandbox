#build with the following command
#docker build -f claude_code_py.Dockerfile \
#        --build-arg HOST_USER_UID=$(id -u) \
#        --build-arg HOST_USER_GID=$(id -g) \
#        --build-arg HOST_USER_NAME=$USER \
#        --build-arg HTTP_PROXY=$HTTP_PROXY \
#        --build-arg HTTPS_PROXY=$HTTPS_PROXY \
#        -t claude_code_py .
FROM python:3.12-slim-bookworm

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
ENV PYTHONUNBUFFERED=1
ENV PYTHONDONTWRYTEBYTECODE=1
ENV UV_COMPILE_BYTECODE=1

RUN echo "Host Name is: ${HOST_USER_NAME}"
RUN echo "Host UID is: ${HOST_USER_UID}"
RUN echo "Host GID is: ${HOST_USER_GID}"

# Use Aliyun mirror for Debian packages
RUN echo "deb http://mirrors.aliyun.com/debian/ bookworm main contrib non-free non-free-firmware" > /etc/apt/sources.list && \
    echo "deb http://mirrors.aliyun.com/debian-security/ bookworm-security main contrib non-free non-free-firmware" >> /etc/apt/sources.list && \
    echo "deb http://mirrors.aliyun.com/debian/ bookworm-updates main contrib non-free non-free-firmware" >> /etc/apt/sources.list

# Install system dependencies including locales
RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    curl \
    sudo \
    build-essential \
    libpq-dev \
    pkg-config \
    ca-certificates \
    locales \
    openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && apt-get clean

# Generate en_US.UTF-8 locale
RUN echo "en_US.UTF-8 UTF-8" > /etc/locale.gen \
    && locale-gen en_US.UTF-8 \
    && update-locale LANG=en_US.UTF-8

ENV LANG=en_US.UTF-8
ENV LC_ALL=en_US.UTF-8

# Create workspace directory with open permissions
RUN mkdir -p /workspace && chmod 777 /workspace

# Create user inside the container with the host's UID/GID
# Handle case where GID already exists
RUN if getent group ${HOST_USER_GID} > /dev/null 2>&1; then \
        EXISTING_GROUP=$(getent group ${HOST_USER_GID} | cut -d: -f1) && \
        useradd --uid ${HOST_USER_UID} --gid ${HOST_USER_GID} -m -s /bin/bash ${HOST_USER_NAME}; \
    else \
        groupadd --gid ${HOST_USER_GID} ${HOST_USER_NAME} && \
        useradd --uid ${HOST_USER_UID} --gid ${HOST_USER_GID} -m -s /bin/bash ${HOST_USER_NAME}; \
    fi

# Create .local/bin directory for the user
RUN mkdir -p /home/${HOST_USER_NAME}/.local/bin && chown -R ${HOST_USER_NAME}:${HOST_USER_GID} /home/${HOST_USER_NAME}/.local

# Switch to user for installing uv and claude
USER ${HOST_USER_NAME}

# Install uv as user
ADD --chown=${HOST_USER_NAME}:${HOST_USER_GID} https://astral.sh/uv/install.sh /tmp/uv-install.sh
RUN sh /tmp/uv-install.sh \
    && rm /tmp/uv-install.sh

# Install Claude Code using official installer
RUN curl -fsSL https://claude.ai/install.sh | bash

# Set working directory
WORKDIR /workspace

# Default command (runs as user)
CMD ["claude"]
