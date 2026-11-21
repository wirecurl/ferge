#!/bin/sh
# Installs the client keys, creates the fixture tree once and runs sshd.
set -eu

for user in ferry ferry-nopasswd ferry-nosudo; do
    install -d -o "$user" -g "$user" -m 0700 "/home/$user/.ssh"
    if [ -f /etc/ferry/authorized_keys ]; then
        install -o "$user" -g "$user" -m 0600 /etc/ferry/authorized_keys "/home/$user/.ssh/authorized_keys"
    fi
done

if [ ! -f /home/ferry/.fixtures-complete ]; then
    su ferry -s /bin/sh -c "python3 /usr/local/bin/generate_fixtures.py /home/ferry"
    touch /home/ferry/.fixtures-complete
fi
touch /run/ferry-fixtures-ready

exec /usr/sbin/sshd -D -e
