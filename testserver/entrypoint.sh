#!/bin/sh
# Creates host keys, installs the client keys, creates the fixture tree once
# (unless FERGE_FIXTURES=none) and runs sshd, or with FERGE_SERVICE=ftp,
# vsftpd with explicit TLS on port 21, implicit TLS on port 990 and none on
# port 2100, and ProFTPD (which knows MLSD) with explicit TLS on port 2101.
# vsftpd takes the lower half of the passive ports, ProFTPD the upper.
set -eu

ssh-keygen -A >/dev/null

for user in ferge ferge-nopasswd ferge-sftponly ferge-nosudo; do
    install -d -o "$user" -g "$user" -m 0700 "/home/$user/.ssh"
    if [ -f /etc/ferge/authorized_keys ]; then
        install -o "$user" -g "$user" -m 0600 /etc/ferge/authorized_keys "/home/$user/.ssh/authorized_keys"
    fi
done

if [ "${FERGE_FIXTURES:-all}" != none ] && [ ! -f /home/ferge/.fixtures-complete ]; then
    su ferge -s /bin/sh -c "python3 /usr/local/bin/generate_fixtures.py /home/ferge"
    touch /home/ferge/.fixtures-complete
fi

if [ "${FERGE_SERVICE:-ssh}" = ftp ]; then
    # A new self-signed certificate for every container, like the host keys,
    # kept when the container restarts, since up.sh saved it for the tests.
    if [ ! -f /etc/ferge-ftp/cert.pem ]; then
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
            -subj /CN=ferge-test -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
            -keyout /etc/ferge-ftp/key.pem -out /etc/ferge-ftp/cert.pem 2>/dev/null
        chmod 0600 /etc/ferge-ftp/key.pem
    fi
    # The configurations in use, made anew from the ones in the image at
    # every start.
    run=/run/ferge-ftp
    install -d "$run" /run/proftpd
    pasv_min=${FERGE_FTP_PASV_MIN:-30000}
    pasv_max=${FERGE_FTP_PASV_MAX:-30039}
    pasv_half=$(( (pasv_min + pasv_max) / 2 ))
    {
        cat /etc/ferge-ftp/vsftpd.conf
        printf 'pasv_min_port=%s\npasv_max_port=%s\n' "$pasv_min" "$pasv_half"
    } > "$run/vsftpd.conf"
    {
        cat /etc/ferge-ftp/proftpd.conf
        printf 'PassivePorts %s %s\n' "$((pasv_half + 1))" "$pasv_max"
    } > "$run/proftpd.conf"
    sed -e 's/^listen_port=21$/listen_port=990/' \
        -e 's/^force_local_logins_ssl=NO$/force_local_logins_ssl=YES/' \
        -e 's/^force_local_data_ssl=NO$/force_local_data_ssl=YES/' \
        "$run/vsftpd.conf" > "$run/vsftpd-implicit.conf"
    echo 'implicit_ssl=YES' >> "$run/vsftpd-implicit.conf"
    # A third one without TLS at all, for the question before a plain login.
    sed -e 's/^listen_port=21$/listen_port=2100/' -e 's/^ssl_enable=YES$/ssl_enable=NO/' \
        "$run/vsftpd.conf" > "$run/vsftpd-plain.conf"
    touch /run/ferge-fixtures-ready
    /usr/sbin/vsftpd "$run/vsftpd-implicit.conf" &
    /usr/sbin/vsftpd "$run/vsftpd-plain.conf" &
    /usr/sbin/proftpd --nodaemon --config "$run/proftpd.conf" &
    exec /usr/sbin/vsftpd "$run/vsftpd.conf"
fi

touch /run/ferge-fixtures-ready
exec /usr/sbin/sshd -D -e
