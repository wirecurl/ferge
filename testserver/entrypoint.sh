#!/bin/sh
# Installs the client keys, creates the fixture tree once and runs sshd.
set -eu

for user in ferge ferge-nopasswd ferge-sftponly ferge-nosudo; do
    install -d -o "$user" -g "$user" -m 0700 "/home/$user/.ssh"
    if [ -f /etc/ferge/authorized_keys ]; then
        install -o "$user" -g "$user" -m 0600 /etc/ferge/authorized_keys "/home/$user/.ssh/authorized_keys"
    fi
done

if [ ! -f /home/ferge/.fixtures-complete ]; then
    su ferge -s /bin/sh -c "python3 /usr/local/bin/generate_fixtures.py /home/ferge"
    touch /home/ferge/.fixtures-complete
fi
touch /run/ferge-fixtures-ready

exec /usr/sbin/sshd -D -e
