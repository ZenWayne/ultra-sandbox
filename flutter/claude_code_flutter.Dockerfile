# Build with:
# podman build -f claude_code_flutter.Dockerfile \
#         --build-arg HOST_USER_UID=$(id -u) \
#         --build-arg HOST_USER_GID=$(id -g) \
#         --build-arg HOST_USER_NAME=$USER \
#         -t claude_code_flutter .
FROM ubuntu:22.04

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

# Flutter China mirrors
ENV FLUTTER_STORAGE_BASE_URL=https://storage.flutter-io.cn
ENV PUB_HOSTED_URL=https://pub.flutter-io.cn

# Android SDK paths
ENV ANDROID_HOME=/opt/android-sdk
ENV ANDROID_SDK_ROOT=/opt/android-sdk
ENV JAVA_HOME=/usr/lib/jvm/java-17-openjdk-amd64
ENV PATH=/opt/flutter/bin:/opt/android-sdk/platform-tools:/opt/android-sdk/cmdline-tools/latest/bin:${JAVA_HOME}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Use Aliyun mirror for Ubuntu packages
RUN echo "deb http://mirrors.aliyun.com/ubuntu/ jammy main restricted universe multiverse" > /etc/apt/sources.list && \
    echo "deb http://mirrors.aliyun.com/ubuntu/ jammy-updates main restricted universe multiverse" >> /etc/apt/sources.list && \
    echo "deb http://mirrors.aliyun.com/ubuntu/ jammy-security main restricted universe multiverse" >> /etc/apt/sources.list

# ── System dependencies (root) ────────────────────────────────────────────────
RUN apt-get update && apt-get install -y --no-install-recommends \
    git curl wget unzip xz-utils zip sudo \
    build-essential ca-certificates locales openssh-client \
    openjdk-17-jdk libglu1-mesa \
    clang cmake ninja-build pkg-config \
    libgtk-3-dev liblzma-dev \
    && rm -rf /var/lib/apt/lists/* && apt-get clean

# Node.js LTS (required for Claude Code)
RUN curl -fsSL https://deb.nodesource.com/setup_lts.x | bash - && \
    apt-get install -y nodejs && \
    rm -rf /var/lib/apt/lists/*

# Claude Code (global, root)
RUN npm install -g @anthropic-ai/claude-code

# Locale
RUN echo "en_US.UTF-8 UTF-8" > /etc/locale.gen \
    && locale-gen en_US.UTF-8 \
    && update-locale LANG=en_US.UTF-8
ENV LANG=en_US.UTF-8
ENV LC_ALL=en_US.UTF-8

# ── Create user early ─────────────────────────────────────────────────────────
RUN if getent group ${HOST_USER_GID} > /dev/null 2>&1; then \
        useradd --uid ${HOST_USER_UID} --gid ${HOST_USER_GID} -m -s /bin/bash ${HOST_USER_NAME}; \
    else \
        groupadd --gid ${HOST_USER_GID} ${HOST_USER_NAME} && \
        useradd --uid ${HOST_USER_UID} --gid ${HOST_USER_GID} -m -s /bin/bash ${HOST_USER_NAME}; \
    fi && \
    echo "${HOST_USER_NAME} ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# Pre-create dirs owned by user
RUN mkdir -p /opt/android-sdk /opt/flutter /workspace \
    /home/${HOST_USER_NAME}/.local/bin \
    /home/${HOST_USER_NAME}/.pub-cache \
    /home/${HOST_USER_NAME}/.flutter && \
    chown -R ${HOST_USER_NAME}:${HOST_USER_GID} \
        /opt/android-sdk /opt/flutter /workspace \
        /home/${HOST_USER_NAME}/.local \
        /home/${HOST_USER_NAME}/.pub-cache \
        /home/${HOST_USER_NAME}/.flutter

# ── Everything below runs as user (no chown layers) ───────────────────────────
USER ${HOST_USER_NAME}

# Android cmdline-tools
RUN mkdir -p /opt/android-sdk/cmdline-tools && \
    wget -q https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip \
         -O /tmp/cmdline-tools.zip && \
    unzip -q /tmp/cmdline-tools.zip -d /tmp/cmdline-tools-tmp && \
    mv /tmp/cmdline-tools-tmp/cmdline-tools /opt/android-sdk/cmdline-tools/latest && \
    rm -rf /tmp/cmdline-tools.zip /tmp/cmdline-tools-tmp

# Android SDK components (Flutter 3.41.2: compileSdk=36, ndk=28.2.13676358)
RUN unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy; \
    yes | sdkmanager --licenses && \
    sdkmanager \
        "platform-tools" \
        "build-tools;35.0.0" \
        "platforms;android-33" \
        "platforms;android-34" \
        "platforms;android-36" \
        "ndk;28.2.13676358"

# Flutter 3.41.2
RUN git clone --depth 1 -b 3.41.2 https://github.com/flutter/flutter.git /opt/flutter

# Flutter init
RUN flutter config --android-sdk /opt/android-sdk --no-analytics && \
    flutter precache --android --linux || true && \
    yes | flutter doctor --android-licenses 2>/dev/null || true && \
    flutter doctor 2>&1 || true

WORKDIR /workspace

CMD ["claude"]
