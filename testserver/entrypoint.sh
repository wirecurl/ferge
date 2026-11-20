#!/bin/sh
# Installs the client keys, creates the fixture tree once and runs sshd.
set -eu

install -d -o ferry -g ferry -m 0700 /home/ferry/.ssh
if [ -f /etc/ferry/authorized_keys ]; then
    install -o ferry -g ferry -m 0600 /etc/ferry/authorized_keys /home/ferry/.ssh/authorized_keys
fi

if [ ! -f /home/ferry/.fixtures-complete ]; then
    su ferry -s /bin/sh -c "python3 /usr/local/bin/generate_fixtures.py /home/ferry"
    touch /home/ferry/.fixtures-complete
fi
touch /run/ferry-fixtures-ready

exec /usr/sbin/sshd -D -e
