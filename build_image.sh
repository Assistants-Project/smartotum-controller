#!/bin/bash

  # smartotum-controller:
  #   image: smartotum-controller:latest
  #   container_name: smartotum-controller
  #   restart: unless-stopped
  #   network_mode: "host"
  #   environment:
  #     - TZ=Europe/Rome

set -e

IMAGE_NAME=smartotum-controller
IMAGE_TAG=latest
PLATFORM=linux/arm64
BINARY_NAME=smartotum-controller

echo "🦀 Compiling Rust binary for ARM64..."
time cargo build --release --target aarch64-unknown-linux-musl

echo "📋 Stripping binary..."
aarch64-linux-gnu-strip target/aarch64-unknown-linux-musl/release/${BINARY_NAME}

echo "📊 Binary size:"
ls -lh target/aarch64-unknown-linux-musl/release/${BINARY_NAME}

cp target/aarch64-unknown-linux-musl/release/${BINARY_NAME} .

echo "🐳 Building Docker image..."
docker buildx build \
  --platform ${PLATFORM} \
  -t ${IMAGE_NAME}:${IMAGE_TAG} \
  -f docker/Dockerfile-OpenWrt \
  --load \
  .

rm ${BINARY_NAME}

echo "📦 Exporting image..."
docker save -o ${IMAGE_NAME}.tar ${IMAGE_NAME}:${IMAGE_TAG}

echo "📤 Upload to gateway..."
scp -O ${IMAGE_NAME}.tar root@10.0.1.1:/opt/docker/tmp

echo "💻 SSH into gateway and load image..."
ssh root@10.0.1.1 << 'EOF'
  docker load -i /opt/docker/tmp/smartotum-controller.tar
  cd /opt/domo/domo-compose
  docker compose stop smartotum-controller
  docker compose up -d smartotum-controller
EOF

echo "✅ Done!"
