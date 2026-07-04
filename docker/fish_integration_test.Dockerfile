FROM ubuntu:24.04

RUN apt-get update && apt-get install -y \
    fish \
    git \
    python3 \
    && rm -rf /var/lib/apt/lists/*

RUN fish --version

COPY --from=built-artifact /flyline-standalone /usr/local/bin/flyline-standalone
COPY scripts/flyline.fish /opt/flyline/flyline.fish
COPY docker/fish_integration_test.py /opt/flyline/test.py

RUN chmod +x /usr/local/bin/flyline-standalone \
    && python3 /opt/flyline/test.py
