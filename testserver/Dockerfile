# OpenSSH test server for Ferry's integration tests and benchmarks.
# Start it with ./testserver/up.sh; see CLAUDE.md.
FROM alpine:3.24

RUN apk add --no-cache openssh-server python3 \
    && ssh-keygen -A \
    && adduser -D -s /bin/sh ferry \
    && echo 'ferry:ferry' | chpasswd

COPY sshd_config /etc/ssh/sshd_config
COPY generate_fixtures.py /usr/local/bin/generate_fixtures.py
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh /usr/local/bin/generate_fixtures.py

EXPOSE 22
HEALTHCHECK --interval=1s --timeout=2s --start-period=120s --retries=3 \
    CMD test -f /run/ferry-fixtures-ready && nc -z 127.0.0.1 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
