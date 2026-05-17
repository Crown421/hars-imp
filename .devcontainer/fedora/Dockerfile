ARG FEDORA_VERSION=latest
FROM fedora:${FEDORA_VERSION}

ARG USERNAME=vscode
ARG USER_UID=1000
ARG USER_GID=${USER_UID}

# Base development utilities: roughly mirrors devcontainers/common-utils for
# Fedora without pulling in project-specific Rust or RPM packaging tools.
RUN dnf upgrade -y \
    && dnf install -y \
        bash-completion \
        ca-certificates \
        curl \
        diffutils \
        dnf-plugins-core \
        file \
        findutils \
        git \
        git-lfs \
        gnupg2 \
        gzip \
        hostname \
        iproute \
        iputils \
        jq \
        less \
        lsof \
        man-db \
        man-pages \
        nano \
        openssh-clients \
        procps-ng \
        ripgrep \
        sudo \
        tar \
        tree \
        unzip \
        which \
        wget \
        xz \
        zip \
        zsh \
    && dnf clean all \
    && groupadd --gid "${USER_GID}" "${USERNAME}" \
    && useradd --uid "${USER_UID}" --gid "${USER_GID}" -m -s /usr/bin/zsh "${USERNAME}" \
    && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/${USERNAME}" \
    && chmod 0440 "/etc/sudoers.d/${USERNAME}"

# Match the non-Rust tools from the existing Debian devcontainer features:
# mise, pre-commit, and chezmoi.
RUN dnf copr enable -y jdxcode/mise \
    && dnf install -y \
        chezmoi \
        mise \
        pre-commit \
    && dnf clean all

# Rust/project development layer. Keep this separate so the Fedora base can be
# reused for non-Rust repos by deleting or replacing this block.
RUN dnf install -y \
        cargo \
        clippy \
        gcc \
        gcc-c++ \
        make \
        mosquitto \
        mosquitto-clients \
        openssl-devel \
        pkgconf-pkg-config \
        rust \
        rust-analyzer \
        rust-src \
        rustfmt \
    && dnf clean all

# RPM/COPR packaging layer. Keep packaging tools isolated from the base/Rust
# setup so this container can be adapted for projects that do not produce RPMs.
RUN dnf install -y \
        copr-cli \
        mock \
        rpm-build \
        rpmdevtools \
        rpmlint \
        rust2rpm \
    && cargo install cargo-generate-rpm --version 0.16.1 --locked --root /usr/local \
    && dnf clean all \
    && usermod -aG mock "${USERNAME}"
