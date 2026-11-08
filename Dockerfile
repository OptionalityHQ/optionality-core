FROM ubuntu:24.04 AS builder

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    ca-certificates \
    git \
    && rm -rf /var/lib/apt/lists/*

# Install Node.js 22 LTS
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Install Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.85.0
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup target add wasm32-unknown-unknown

WORKDIR /app
COPY . .

# Install dependencies and build demo bundle
RUN npm install
RUN node sim/build-demo.mjs

# Build release binaries for engine, chain-host, oracle-signer
RUN cargo build --release -p engine -p chain-host -p oracle-signer

FROM ubuntu:24.04 AS runner

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    libssl3 \
    procps \
    lsof \
    && rm -rf /var/lib/apt/lists/*

# Install Node.js runtime
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/package.json ./
COPY --from=builder /app/node_modules ./node_modules
COPY --from=builder /app/target/release/engine ./target/release/engine
COPY --from=builder /app/target/release/chain-host ./target/release/chain-host
COPY --from=builder /app/target/release/oracle-signer ./target/release/oracle-signer
COPY --from=builder /app/target/near ./target/near
COPY --from=builder /app/api ./api
COPY --from=builder /app/demo ./demo
COPY --from=builder /app/sim ./sim
COPY --from=builder /app/scripts ./scripts
COPY --from=builder /app/data ./data

ENV ENGINE_PORT=8791
ENV GATEWAY_PORT=8790
ENV PROFILE=release
ENV SKIP_BUILD=1
ENV FOREGROUND=1

EXPOSE 8790

CMD ["./scripts/venue.sh", "foreground"]
