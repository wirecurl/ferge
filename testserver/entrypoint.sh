#!/bin/sh
# Creates host keys, installs the client keys, creates the fixture tree once
# (unless FERGE_FIXTURES=none) and runs sshd, or with FERGE_SERVICE=ftp,
# vsftpd with explicit TLS on port 21, implicit TLS on port 990 and none on
# port 2100.
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
    # A new self-signed certificate for every container, like the host keys.
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 3650 \
        -subj /CN=ferge-test -addext subjectAltName=DNS:localhost,IP:127.0.0.1 \
        -keyout /etc/ferge-ftp/key.pem -out /etc/ferge-ftp/cert.pem 2>/dev/null
    chmod 0600 /etc/ferge-ftp/key.pem
    pasv="pasv_min_port=${FERGE_FTP_PASV_MIN:-30000}
pasv_max_port=${FERGE_FTP_PASV_MAX:-30019}"
    printf '%s\n' "$pasv" >> /etc/ferge-ftp/vsftpd.conf
    sed -e 's/^listen_port=21$/listen_port=990/' \
        -e 's/^force_local_logins_ssl=NO$/force_local_logins_ssl=YES/' \
        -e 's/^force_local_data_ssl=NO$/force_local_data_ssl=YES/' \
        /etc/ferge-ftp/vsftpd.conf > /etc/ferge-ftp/vsftpd-implicit.conf
    echo 'implicit_ssl=YES' >> /etc/ferge-ftp/vsftpd-implicit.conf
    # A third one without TLS at all, for the question before a plain login.
    sed -e 's/^listen_port=21$/listen_port=2100/' -e 's/^ssl_enable=YES$/ssl_enable=NO/' \
        /etc/ferge-ftp/vsftpd.conf > /etc/ferge-ftp/vsftpd-plain.conf
    touch /run/ferge-fixtures-ready
    /usr/sbin/vsftpd /etc/ferge-ftp/vsftpd-implicit.conf &
    /usr/sbin/vsftpd /etc/ferge-ftp/vsftpd-plain.conf &
    exec /usr/sbin/vsftpd /etc/ferge-ftp/vsftpd.conf
fi

touch /run/ferge-fixtures-ready
exec /usr/sbin/sshd -D -e
