FROM node:22-bookworm

ENV DEBIAN_FRONTEND=noninteractive
ENV PATH=/root/.cargo/bin:$PATH

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    file \
    libayatana-appindicator3-dev \
    librsvg2-dev \
    libsecret-1-dev \
    libwebkit2gtk-4.1-dev \
    patchelf \
    pkg-config \
    xdg-utils \
    xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

WORKDIR /work
COPY . .

RUN npm ci
RUN cargo test --workspace
RUN npm run typecheck
RUN cargo build --release -p dakia-cli
RUN npm run build -- --bundles deb,appimage
