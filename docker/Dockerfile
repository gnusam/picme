# Debian 13 (trixie) : glibc et libs plus anciennes que Manjaro, le binaire construit ici tourne sur le poste
FROM rust:1-trixie

RUN apt-get update && apt-get install -y --no-install-recommends \
    # build
    pkg-config libgtk-4-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    libheif-dev libturbojpeg0-dev \
    # runtime et fixtures
    libheif-plugin-libde265 libheif-plugin-dav1d libheif-plugin-x265 libheif-plugin-aomenc \
    libheif-examples libjxl-tools \
    gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav gstreamer1.0-gl \
    gstreamer1.0-tools libgl1-mesa-dri \
    # outils de fixtures et de verification X11
    imagemagick libimage-exiftool-perl libjpeg-turbo-progs x11-utils x11-xserver-utils xdotool \
    && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

ENV CARGO_HOME=/cargo
RUN mkdir -p /cargo && chmod 1777 /cargo

WORKDIR /work
