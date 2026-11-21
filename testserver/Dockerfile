# OpenSSH test server for ferge's integration tests and benchmarks.
# Start it with ./testserver/up.sh; see CLAUDE.md.
FROM alpine:3.24

# Working as root: ferge may use sudo with its password, ferge-nopasswd without
# one, and ferge-nosudo not at all. sftp-server is the program sudo runs.
RUN apk add --no-cache openssh-server openssh-sftp-server python3 sudo \
    && ssh-keygen -A \
    && for user in ferge ferge-nopasswd ferge-nosudo; do \
           adduser -D -s /bin/sh "$user" && echo "$user:ferge" | chpasswd; \
       done \
    && echo 'ferge ALL=(ALL) ALL' > /etc/sudoers.d/ferge \
    && echo 'ferge-nopasswd ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/ferge-nopasswd \
    && chmod 0440 /etc/sudoers.d/ferge /etc/sudoers.d/ferge-nopasswd \
    && install -d -m 0700 /root/private \
    && echo 'only root can read this' > /root/private/secret.txt

COPY sshd_config /etc/ssh/sshd_config
COPY generate_fixtures.py /usr/local/bin/generate_fixtures.py
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh /usr/local/bin/generate_fixtures.py

EXPOSE 22
HEALTHCHECK --interval=1s --timeout=2s --start-period=120s --retries=3 \
    CMD test -f /run/ferge-fixtures-ready && nc -z 127.0.0.1 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
